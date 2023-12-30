#![no_main]

use anyhow::Result;

use bstr::{BStr, BString};

use gix_config::{
    file::{init::Options, Metadata},
    File,
};
use libfuzzer_sys::fuzz_target;
use std::hint::black_box;
use std::str;

fn fuzz_immutable(file: &File, section_name: &str, subsection_name: &Option<BString>, key: &str) {
    // Can't use the map here is the borrow checker chucks a flip converting between
    // &Option<BString> and Option<&BStr>.
    let subsection_name: Option<&BStr> = if let Some(n) = subsection_name {
        Some(n.as_ref())
    } else {
        None
    };
    // Singular strings.
    _ = black_box(file.string(section_name, subsection_name, key));
    _ = black_box(file.string_by_key(key));
    _ = black_box(file.string_filter(section_name, subsection_name, key, &mut |_| false));
    _ = black_box(file.string_filter_by_key(key, &mut |_| false));

    // Plural strings.
    _ = black_box(file.strings(section_name, subsection_name, key));
    _ = black_box(file.strings_by_key(key));
    _ = black_box(file.strings_filter(section_name, subsection_name, key, &mut |_| false));
    _ = black_box(file.strings_filter_by_key(key, &mut |_| false));

    // Singular path.
    _ = black_box(file.path(section_name, subsection_name, key));
    _ = black_box(file.path_by_key(key));
    _ = black_box(file.path_filter(section_name, subsection_name, key, &mut |_| false));
    _ = black_box(file.path_filter_by_key(key, &mut |_| false));

    // Singular bool.
    _ = black_box(file.boolean(section_name, subsection_name, key));
    _ = black_box(file.boolean_by_key(key));
    _ = black_box(file.boolean_filter(section_name, subsection_name, key, &mut |_| false));
    _ = black_box(file.boolean_filter_by_key(key, &mut |_| false));
    // NOTE: no plural bool.

    // Singular integer.
    _ = black_box(file.integer(section_name, subsection_name, key));
    _ = black_box(file.integer_by_key(key));
    _ = black_box(file.integer_filter(section_name, subsection_name, key, &mut |_| false));
    _ = black_box(file.integer_filter_by_key(key, &mut |_| false));

    // Plural integers.
    _ = black_box(file.integers(section_name, subsection_name, key));
    _ = black_box(file.integers_by_key(key));
    _ = black_box(file.integers_filter(section_name, subsection_name, key, &mut |_| false));
    _ = black_box(file.integers_filter_by_key(key, &mut |_| false));
}

fn fuzz_mutable(file: &mut File, section_name: &str, subsection_name: &Option<BString>, key: &str) -> Result<()> {
    // TODO: It might make sense to make fuzzed modifications.

    // Can't use the map here is the borrow checker chucks a flip converting between
    // &Option<BString> and Option<&BStr>.
    let subsection_name: Option<&BStr> = if let Some(n) = subsection_name {
        Some(n.as_ref())
    } else {
        None
    };

    // Mutate section.
    {
        let mut section = file.section_mut(section_name, subsection_name)?;
        section.push_newline();
        section.set(key.to_string().try_into()?, BStr::new("Set value"));
        section.push_newline();
        let kv_pair = section.pop().map(|(key, value)| (key.to_owned(), value.to_owned()));
        if let Some((key, value)) = kv_pair {
            section.push_with_comment(key, Some(&value), "Popped");
        }
    }

    // Singular raw.
    _ = black_box(
        file.raw_value_mut(section_name, subsection_name, key)?
            .set_string("raw_values"),
    );

    // Plural raw.
    _ = black_box(file.raw_values_mut(section_name, subsection_name, key)?.len());
    Ok(())
}

fn fuzz(input: &[u8]) -> Result<()> {
    let meta = Metadata::default();
    let options = Options::default();
    let mut file = File::from_bytes_no_includes(input, meta.clone(), options.clone())?;

    let section_triples: Vec<_> = file
        .sections()
        .flat_map(|sec| {
            sec.keys().map(|key| {
                let section_name = str::from_utf8(sec.header().name()).unwrap();
                let subsection_name = sec.header().subsection_name();
                let key = str::from_utf8(&key).unwrap();
                return (
                    section_name.to_owned(),
                    subsection_name.map(|x| x.to_owned()),
                    key.to_owned(),
                );
            })
        })
        .collect();

    for section_triple in section_triples.iter() {
        let (section_name, subsection_name, key) = section_triple;
        black_box(fuzz_immutable(&file, &section_name, &subsection_name, &key));
    }

    for section_triple in section_triples.iter() {
        let (section_name, subsection_name, key) = section_triple;
        _ = black_box(fuzz_mutable(&mut file, &section_name, &subsection_name, &key));
    }

    _ = black_box(file.sections_and_ids().count());
    _ = black_box(file.sections_and_postmatter().count());
    _ = black_box(file.sections_by_name("section").map(|x| x.count()));
    _ = black_box(file.frontmatter());

    let roundtrip_as_string: Vec<u8> = file.to_bstring().into();
    _ = black_box(File::from_bytes_no_includes(
        &roundtrip_as_string,
        meta.clone(),
        options.clone(),
    ))?;
    Ok(())
}

fuzz_target!(|input: &[u8]| {
    _ = black_box(fuzz(input));
});
