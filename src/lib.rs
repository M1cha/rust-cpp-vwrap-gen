use std::convert::TryInto;
use std::io::Read;
use std::io::Seek;
use std::io::Write;

fn gen_method(
    mut outfile: &std::fs::File,
    mut outfile_hdr: &std::fs::File,
    class: &clang::Entity,
    method: &clang::Entity,
) -> std::io::Result<()> {
    let mut outbuf = Vec::new();
    let mut method_name = method.get_name().unwrap();

    if let clang::EntityKind::Destructor = method.get_kind() {
        method_name = "destructor".to_string();
    };

    if let clang::EntityKind::Constructor = method.get_kind() {
        method_name = "constructor".to_string();
    };

    // write sig to outbuf
    write!(
        &mut outbuf,
        "{} vwrap_{}_{}({} *thiz",
        method.get_result_type().unwrap().get_display_name(),
        class.get_type().unwrap().get_display_name(),
        method_name,
        class.get_type().unwrap().get_display_name()
    )?;
    if let Some(arguments) = method.get_arguments() {
        for argument in arguments {
            write!(&mut outbuf, ", ")?;

            let range = argument.get_range().unwrap();
            let loc_start = range.get_start().get_spelling_location();
            let loc_end = range.get_end().get_spelling_location();
            assert!(loc_start.file == loc_end.file);
            assert!(loc_start.offset < loc_end.offset);
            let filepath = loc_start.file.unwrap().get_path();

            let mut file = std::fs::File::open(filepath).unwrap();
            file.seek(std::io::SeekFrom::Start(loc_start.offset.into()))
                .unwrap();
            let mut buf = vec![0; (loc_end.offset - loc_start.offset).try_into().unwrap()];
            file.read_exact(&mut buf)?;
            outbuf.write_all(&buf)?;
        }
    }
    write!(&mut outbuf, ")")?;

    // write to header
    outfile_hdr.write_all(&outbuf)?;
    writeln!(&mut outfile_hdr, ";")?;

    // write to sourcefile
    outfile.write_all(&outbuf)?;
    writeln!(&mut outfile, " {{")?;

    write!(&mut outfile, "    ")?;
    if method.get_result_type().unwrap().get_kind() != clang::TypeKind::Void {
        write!(&mut outfile, "return ")?;
    }

    if let clang::EntityKind::Constructor = method.get_kind() {
        write!(&mut outfile, "new(thiz) ")?;
    } else {
        write!(&mut outfile, "thiz->")?;
    }
    write!(&mut outfile, "{}(", method.get_name().unwrap())?;
    if let Some(arguments) = method.get_arguments() {
        let mut first = true;
        for argument in arguments {
            if first {
                first = false;
            } else {
                write!(&mut outfile, ", ")?;
            }

            write!(&mut outfile, "{}", argument.get_display_name().unwrap())?;
        }
    }
    writeln!(&mut outfile, ");")?;

    writeln!(&mut outfile, "}}")?;

    Ok(())
}

pub fn generate<F: Into<std::path::PathBuf>, S: AsRef<str>>(
    outpath_src: F,
    outpath_hdr: F,
    inpath: F,
    arguments: &[S],
) -> std::io::Result<()> {
    let clang = clang::Clang::new().unwrap();
    let index = clang::Index::new(&clang, false, false);

    let inpath2 = inpath.into();
    let outpath_hdr2 = outpath_hdr.into();
    let mut parser = index.parser(&inpath2);
    let mut clang_args: Vec<std::string::String> = Vec::new();

    if let Some(clang) = clang_sys::support::Clang::find(None, &[]) {
        if let Some(paths) = clang.cpp_search_paths {
            for path in paths.into_iter() {
                clang_args.push("-isystem".to_string());
                clang_args.push(path.to_str().unwrap().to_string());
            }
        }
    }

    for arg in arguments {
        clang_args.push(arg.as_ref().to_string());
    }

    parser.arguments(&clang_args);

    let tu = parser.parse().unwrap();
    let mut has_diag = false;
    for diag in &tu.get_diagnostics() {
        eprintln!("{}", diag.formatter().format());
        has_diag = true;
    }
    if has_diag {
        std::process::exit(1);
    }

    let mut outfile = std::fs::File::create(outpath_src.into())?;
    let mut outfile_hdr = std::fs::File::create(&outpath_hdr2)?;

    writeln!(&mut outfile_hdr, "#pragma once")?;
    writeln!(&mut outfile_hdr, "#include \"{}\"", inpath2.display())?;

    writeln!(&mut outfile, "#include \"{}\"", outpath_hdr2.display())?;
    writeln!(&mut outfile, "#include <new>")?;

    // create list of all classes we found
    let mut classes = std::collections::HashMap::new();
    for class in tu.get_entity().get_children() {
        if class.get_kind() != clang::EntityKind::ClassDecl {
            continue;
        }

        let type_ = class.get_type().unwrap();
        classes.insert(type_.get_display_name(), class);
    }

    for class in classes.values() {
        let type_ = class.get_type().unwrap();
        let mut base_classes = Vec::new();
        let mut changed = true;

        // create list of all classes this inherits from
        base_classes.push(type_.get_display_name());
        while changed {
            changed = false;

            for entity in class.get_children() {
                if entity.get_kind() != clang::EntityKind::BaseSpecifier {
                    continue;
                }

                let display_name = entity.get_type().unwrap().get_display_name();
                if !base_classes.contains(&display_name) {
                    base_classes.push(display_name);
                    changed = true;
                }
            }
        }

        for base_class_name in &base_classes {
            let base_class = classes.get(base_class_name).unwrap();

            for method in &base_class.get_children() {
                match method.get_kind() {
                    clang::EntityKind::Method => {
                        // these are the types we currently can't access properly using bindgen
                        // we generate wrappers for all inherited functions because we can't cast to the
                        // base type without generating conversion wrappers.
                        if class == base_class
                            && !method.is_virtual_method()
                            && !method.is_inline_function()
                        {
                            continue;
                        }
                        // by ignoring overriding methods we ensure we generate one wrapper only,
                        // by generting it for the original only.
                        if method.get_overridden_methods().is_some() {
                            continue;
                        }
                        // we can't access non-public methods from a C wrapper.
                        // XXX: this will go wrong with overriding methods changing visibility
                        if method.get_accessibility().unwrap() != clang::Accessibility::Public {
                            continue;
                        }
                    }
                    // generate wrappers for public constructors/destructors of the current class
                    clang::EntityKind::Destructor | clang::EntityKind::Constructor => {
                        if class != base_class {
                            continue;
                        }

                        if class.is_abstract_record() {
                            continue;
                        }

                        if method.get_accessibility().unwrap() != clang::Accessibility::Public {
                            continue;
                        }
                    }
                    _ => continue,
                }

                gen_method(&outfile, &outfile_hdr, &class, &method)?;
            }
        }
        writeln!(&mut outfile)?;
    }
    Ok(())
}
