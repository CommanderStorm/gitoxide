use super::db;
use crate::corpus;
use crate::corpus::{Engine, Task};
use crate::organize::find_git_repository_workdirs;
use anyhow::{bail, Context};
use bytesize::ByteSize;
use gix::Progress;
use rusqlite::params;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

impl Engine {
    /// Open the corpus DB or create it.
    pub fn open_or_create(db: PathBuf, gitoxide_version: String, progress: corpus::Progress) -> anyhow::Result<Engine> {
        let con = crate::corpus::db::create(db).context("Could not open or create database")?;
        Ok(Engine {
            progress,
            con,
            gitoxide_version,
        })
    }

    /// Run on the existing set of repositories we have already seen or obtain them from `path` if there is none yet.
    pub fn run(&mut self, corpus_path: PathBuf, threads: Option<usize>) -> anyhow::Result<()> {
        let (corpus_path, corpus_id) = self.prepare_corpus_path(corpus_path)?;
        let gitoxide_id = self.gitoxide_version_id_or_insert()?;
        let runner_id = self.runner_id_or_insert()?;
        let repos = self.find_repos_or_insert(&corpus_path, corpus_id)?;
        let tasks = self.tasks_or_insert()?;
        self.perform_run(&corpus_path, gitoxide_id, runner_id, &tasks, repos, threads)
    }

    pub fn refresh(&mut self, corpus_path: PathBuf) -> anyhow::Result<()> {
        let (corpus_path, corpus_id) = self.prepare_corpus_path(corpus_path)?;
        let repos = self.refresh_repos(&corpus_path, corpus_id)?;
        self.progress.set_name("refresh repos");
        self.progress.info(format!(
            "Added or updated {} repositories under {corpus_path:?}",
            repos.len()
        ));
        Ok(())
    }
}

impl Engine {
    fn perform_run(
        &mut self,
        corpus_path: &Path,
        gitoxide_id: db::Id,
        runner_id: db::Id,
        tasks: &[(db::Id, &'static Task)],
        mut repos: Vec<db::Repo>,
        threads: Option<usize>,
    ) -> anyhow::Result<()> {
        let start = Instant::now();
        let task_progress = &mut self.progress;
        task_progress.set_name("run");
        task_progress.init(Some(tasks.len()), gix::progress::count("tasks"));
        let threads = gix::parallel::num_threads(threads);
        for (task_id, task) in tasks {
            let task_start = Instant::now();
            let mut repo_progress = task_progress.add_child(format!("run '{}'", task.short_name));
            repo_progress.init(Some(repos.len()), gix::progress::count("repos"));

            if task.execute_exclusive || threads == 1 {
                let mut run_progress = repo_progress.add_child("set later");
                for repo in &repos {
                    if gix::interrupt::is_triggered() {
                        bail!("interrupted by user");
                    }
                    run_progress.set_name(format!(
                        "{}",
                        repo.path
                            .strip_prefix(corpus_path)
                            .expect("corpus contains repo")
                            .display()
                    ));
                    let mut run = Self::insert_run(&self.con, gitoxide_id, runner_id, *task_id, repo.id)?;
                    task.perform(
                        &mut run,
                        &repo.path,
                        &mut run_progress,
                        Some(threads),
                        &gix::interrupt::IS_INTERRUPTED,
                    );
                    Self::update_run(&self.con, run)?;
                    repo_progress.inc();
                }
                repo_progress.show_throughput(task_start);
            } else {
                let counter = repo_progress.counter();
                let repo_progress = gix::threading::OwnShared::new(gix::threading::Mutable::new(repo_progress));
                gix::parallel::in_parallel_with_slice(
                    &mut repos,
                    Some(threads),
                    {
                        let shared_repo_progress = repo_progress.clone();
                        let path = self.con.path().expect("opened from path on disk").to_owned();
                        move |tid| {
                            (
                                gix::threading::lock(&shared_repo_progress).add_child(format!("{tid}")),
                                rusqlite::Connection::open(&path),
                            )
                        }
                    },
                    |repo, (progress, con), _threads_left, should_interrupt| -> anyhow::Result<()> {
                        progress.set_name(format!(
                            "{}",
                            repo.path
                                .strip_prefix(corpus_path)
                                .expect("corpus contains repo")
                                .display()
                        ));
                        let con = match con {
                            Ok(con) => con,
                            Err(err) => {
                                progress.fail(format!("{err:#?}"));
                                should_interrupt.store(true, Ordering::SeqCst);
                                return Ok(());
                            }
                        };
                        let mut run = Self::insert_run(con, gitoxide_id, runner_id, *task_id, repo.id)?;
                        task.perform(&mut run, &repo.path, progress, Some(1), should_interrupt);
                        Self::update_run(con, run)?;
                        if let Some(counter) = counter.as_ref() {
                            counter.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(())
                    },
                    || (!gix::interrupt::is_triggered()).then(|| Duration::from_millis(100)),
                    std::convert::identity,
                )?;
                gix::threading::lock(&repo_progress).show_throughput(task_start);
            }

            task_progress.inc();
        }
        task_progress.show_throughput(start);
        Ok(())
    }

    fn prepare_corpus_path(&self, corpus_path: PathBuf) -> anyhow::Result<(PathBuf, db::Id)> {
        let corpus_path = gix::path::realpath(corpus_path)?;
        let corpus_id = self.corpus_id_or_insert(&corpus_path)?;
        Ok((corpus_path, corpus_id))
    }

    fn find_repos(&mut self, corpus_path: &Path, corpus_id: db::Id) -> anyhow::Result<Vec<db::Repo>> {
        self.progress.set_name("query db-repos");
        self.progress.init(None, gix::progress::count("repos"));

        Ok(self
            .con
            .prepare("SELECT id, rela_path, odb_size, num_objects, num_references FROM repository WHERE corpus = ?1")?
            .query_map([corpus_id], |r| {
                Ok(db::Repo {
                    id: r.get(0)?,
                    path: corpus_path.join(r.get::<_, String>(1)?),
                    odb_size: ByteSize(r.get(2)?),
                    num_objects: r.get(3)?,
                    num_references: r.get(4)?,
                })
            })?
            .inspect(|_| self.progress.inc())
            .collect::<Result<_, _>>()?)
    }

    fn refresh_repos(&mut self, corpus_path: &Path, corpus_id: db::Id) -> anyhow::Result<Vec<db::Repo>> {
        let start = Instant::now();
        self.progress.set_name("refresh");
        self.progress.init(None, gix::progress::count("repos"));

        let repos = std::thread::scope({
            let progress = &mut self.progress;
            let con = &mut self.con;
            |scope| -> anyhow::Result<_> {
                let threads = std::thread::available_parallelism()
                    .map(std::num::NonZeroUsize::get)
                    .ok()
                    .unwrap_or(1);
                let (path_tx, repo_rx) = {
                    let (path_tx, path_rx) = crossbeam_channel::bounded(threads * 2);
                    let (repo_tx, repo_rx) = std::sync::mpsc::channel::<(PathBuf, anyhow::Result<db::Repo>)>();
                    (0..threads).for_each(|_| {
                        scope.spawn({
                            let path_rx = path_rx.clone();
                            let repo_tx = repo_tx.clone();
                            move || -> anyhow::Result<_> {
                                for repo_path in path_rx {
                                    let res = (|| {
                                        let repo = gix::open_opts(&repo_path, gix::open::Options::isolated())?;
                                        db::Repo::try_from(&repo)
                                    })();
                                    repo_tx.send((repo_path, res))?;
                                }
                                Ok(())
                            }
                        });
                    });
                    (path_tx, repo_rx)
                };

                let find_progress = progress.add_child("find");
                let write_db = scope.spawn(move || -> anyhow::Result<Vec<db::Repo>> {
                    progress.set_name("write to DB");
                    progress.init(None, gix::progress::count("repos"));

                    let mut out = Vec::new();
                    let transaction = con.transaction()?;
                    let mut statement = transaction.prepare("INSERT INTO repository (rela_path, corpus, odb_size, num_objects, num_references) VALUES (?1, ?2, ?3, ?4, ?5)\
                                                    ON CONFLICT DO UPDATE SET rela_path = rela_path, corpus = corpus, odb_size = ?3, num_objects = ?4, num_references = ?5\
                                                    RETURNING id")?;
                    for (repo_path, repo_res) in repo_rx {
                        match repo_res {
                            Ok(mut repo) => {
                                let rela_path = repo.path.strip_prefix(corpus_path)?;
                                repo.id = statement.query_row(params![rela_path.to_str().context("only valid UTF8 is allowed for repository paths")?, corpus_id, repo.odb_size.as_u64(), repo.num_objects, repo.num_references], |r| r.get(0))?;
                                out.push(repo);
                                progress.inc();
                            }
                            Err(err) => progress.fail(format!("{repo_path:?}: {err:#?}")),
                        }
                    }
                    statement.finalize()?;
                    transaction.commit()?;
                    progress.show_throughput(start);
                    Ok(out)
                });

                let repos = gix::interrupt::Iter::new(
                    find_git_repository_workdirs(corpus_path, find_progress, false, Some(threads)),
                    || anyhow::anyhow!("interrupted by user"),
                );
                for res in repos {
                    let (repo_path, _kind) = res?;
                    path_tx.send(repo_path)?;
                }
                drop(path_tx);
                write_db.join().expect("no panic")
            }
        })?;

        Ok(repos)
    }

    fn find_repos_or_insert(&mut self, corpus_path: &Path, corpus_id: db::Id) -> anyhow::Result<Vec<db::Repo>> {
        let start = Instant::now();
        let repos = self.find_repos(corpus_path, corpus_id)?;
        if repos.is_empty() {
            self.refresh_repos(corpus_path, corpus_id)
        } else {
            self.progress.show_throughput(start);
            Ok(repos)
        }
    }
}