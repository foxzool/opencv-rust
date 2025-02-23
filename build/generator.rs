use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Instant;
use std::{env, fs, io, thread};

use crate::docs::transfer_bindings_to_docs;

use super::{files_with_extension, Library, Result, HOST_TRIPLE, MODULES, OUT_DIR, SRC_CPP_DIR, SRC_DIR};

fn is_type_file(path: &Path, module: &str) -> bool {
	path.file_stem().and_then(OsStr::to_str).map_or(false, |stem| {
		let mut stem_chars = stem.chars();
		(&mut stem_chars).take(3).all(|c| c.is_ascii_digit()) && // first 3 chars are digits
			matches!(stem_chars.next(), Some('-')) && // dash
			module.chars().zip(&mut stem_chars).all(|(m, s)| m == s) && // module name
			matches!(stem_chars.next(), Some('-')) && // dash
			stem.ends_with(".type") // ends with ".type"
	})
}

fn copy_indent(mut read: impl BufRead, mut write: impl Write, indent: &str) -> Result<()> {
	let mut line = Vec::with_capacity(100);
	while read.read_until(b'\n', &mut line)? != 0 {
		write.write_all(indent.as_bytes())?;
		write.write_all(&line)?;
		line.clear();
	}
	Ok(())
}

fn file_move_to_dir(src_file: &Path, target_dir: &Path) -> Result<PathBuf> {
	if !target_dir.exists() {
		fs::create_dir_all(target_dir)?;
	}
	let src_filename = src_file.file_name().ok_or("Can't calculate filename")?;
	let target_file = target_dir.join(src_filename);
	// rename doesn't work across fs boundaries for example
	if fs::rename(src_file, &target_file).is_err() {
		fs::copy(src_file, &target_file)?;
		fs::remove_file(src_file)?;
	}
	Ok(target_file)
}

pub fn gen_wrapper(
	opencv_header_dir: &Path,
	opencv: &Library,
	job_server: jobserver::Client,
	mut generator_build: Child,
) -> Result<()> {
	let target_docs_dir = env::var_os("OCVRS_DOCS_GENERATE_DIR").map(PathBuf::from);
	let target_hub_dir = OUT_DIR.join("opencv");
	let target_module_dir = target_hub_dir.join("hub");
	let manual_dir = SRC_DIR.join("manual");

	eprintln!("=== Generating code in: {}", OUT_DIR.display());
	eprintln!("=== Placing generated bindings into: {}", target_hub_dir.display());
	if let Some(target_docs_dir) = target_docs_dir.as_ref() {
		eprintln!(
			"=== Placing static generated docs bindings into: {}",
			target_docs_dir.display()
		);
	}
	eprintln!("=== Using OpenCV headers from: {}", opencv_header_dir.display());

	for entry in OUT_DIR.read_dir()?.flatten() {
		let path = entry.path();
		if path.is_file() && path.extension().map_or(true, |ext| !ext.eq_ignore_ascii_case("dll")) {
			let _ = fs::remove_file(path);
		}
	}

	let additional_include_dirs = opencv
		.include_paths
		.iter()
		.filter(|&include_path| include_path != opencv_header_dir)
		.cloned()
		.collect::<Vec<_>>();

	let clang = clang::Clang::new().expect("Cannot initialize clang");
	eprintln!("=== Clang: {}", clang::get_version());
	let gen = opencv_binding_generator::Generator::new(opencv_header_dir, &additional_include_dirs, &SRC_CPP_DIR, clang);
	eprintln!("=== Clang command line args: {:#?}", gen.build_clang_command_line_args());

	eprintln!("=== Building binding-generator binary:");
	if let Some(child_stderr) = generator_build.stderr.take() {
		for line in BufReader::new(child_stderr).lines().flatten() {
			eprintln!("=== {line}");
		}
	}
	if let Some(child_stdout) = generator_build.stdout.take() {
		for line in BufReader::new(child_stdout).lines().flatten() {
			eprintln!("=== {line}");
		}
	}
	let child_status = generator_build.wait()?;
	if !child_status.success() {
		return Err("Failed to build the bindings generator".into());
	}

	let additional_include_dirs = Arc::new(
		additional_include_dirs
			.iter()
			.cloned()
			.map(|p| {
				p.to_str()
					.expect("Can't convert additional include dir to UTF-8 string")
					.to_string()
			})
			.collect::<Vec<_>>(),
	);
	let opencv_header_dir = Arc::new(opencv_header_dir.to_owned());
	let modules = MODULES.get().expect("MODULES not initialized");
	let mut join_handles = Vec::with_capacity(modules.len());
	let start = Instant::now();
	modules.iter().for_each(|module| {
		let token = job_server.acquire().expect("Can't acquire token from job server");
		let join_handle = thread::spawn({
			let additional_include_dirs = Arc::clone(&additional_include_dirs);
			let opencv_header_dir = Arc::clone(&opencv_header_dir);
			move || {
				let mut bin_generator = match HOST_TRIPLE.as_ref() {
					Some(host_triple) => Command::new(OUT_DIR.join(format!("{host_triple}/release/binding-generator"))),
					None => Command::new(OUT_DIR.join("release/binding-generator")),
				};
				bin_generator
					.arg(&*opencv_header_dir)
					.arg(&*SRC_CPP_DIR)
					.arg(&*OUT_DIR)
					.arg(module)
					.arg(additional_include_dirs.join(","));
				eprintln!("=== Running: {bin_generator:#?}");
				let res = bin_generator.status().expect("Can't run bindings generator");
				if !res.success() {
					panic!("Failed to run the bindings generator");
				}
				eprintln!("=== Generated: {module}");
				drop(token); // needed to move the token to the thread
			}
		});
		join_handles.push(join_handle);
	});
	for join_handle in join_handles {
		join_handle.join().expect("Generator thread panicked");
	}
	eprintln!("=== Total binding generation time: {:?}", start.elapsed());

	if !target_hub_dir.exists() {
		fs::create_dir(&target_hub_dir)?;
	}
	if !target_module_dir.exists() {
		fs::create_dir(&target_module_dir)?;
	}
	if let Some(target_docs_dir) = target_docs_dir.as_ref().filter(|dir| !dir.exists()) {
		fs::create_dir(target_docs_dir)?;
	}
	for path in files_with_extension(&target_module_dir, "rs")? {
		let _ = fs::remove_file(path);
	}

	fn write_has_module(mut write: impl Write, module: &str) -> Result<()> {
		Ok(writeln!(write, "#[cfg(ocvrs_has_module_{module})]")?)
	}

	fn write_module_path(mut write: impl Write, target_module_dir: &Path, module: &str) -> Result<()> {
		Ok(writeln!(
			write,
			"#[path = r\"{}\"]",
			target_module_dir
				.join(format!("{module}.rs"))
				.to_str()
				.expect("Can't convert path to str")
		)?)
	}

	let add_manual = |file: &mut BufWriter<File>, module: &str| -> Result<bool> {
		if manual_dir.join(format!("{module}.rs")).exists() {
			writeln!(file, "pub use crate::manual::{module}::*;")?;
			Ok(true)
		} else {
			Ok(false)
		}
	};

	// block to close file handles before copying the bindings
	{
		let mut hub_rs = BufWriter::new(File::create(target_hub_dir.join("hub.rs"))?);

		let mut types_rs = BufWriter::new(File::create(target_module_dir.join("types.rs"))?);
		writeln!(types_rs)?;

		let mut sys_rs = BufWriter::new(File::create(target_module_dir.join("sys.rs"))?);
		writeln!(sys_rs, "use crate::{{mod_prelude_sys::*, core}};")?;
		writeln!(sys_rs)?;

		for module in modules {
			// merge multiple *-type.cpp files into a single module_types.hpp
			let module_cpp = OUT_DIR.join(format!("{module}.cpp"));
			if module_cpp.is_file() {
				let module_types_cpp = OUT_DIR.join(format!("{module}_types.hpp"));
				let mut module_types_file = BufWriter::new(
					OpenOptions::new()
						.create(true)
						.truncate(true)
						.write(true)
						.open(module_types_cpp)?,
				);
				let mut type_files = files_with_extension(&OUT_DIR, "cpp")?
					.filter(|f| is_type_file(f, module))
					.collect::<Vec<_>>();
				type_files.sort_unstable();
				for entry in type_files {
					io::copy(&mut BufReader::new(File::open(&entry)?), &mut module_types_file)?;
					let _ = fs::remove_file(entry);
				}
			}

			// add module entry to hub.rs and move the file into opencv/hub
			write_has_module(&mut hub_rs, module)?;
			write_module_path(&mut hub_rs, &target_module_dir, module)?;
			writeln!(hub_rs, "pub mod {module};")?;
			let module_filename = format!("{module}.rs");
			let target_file = file_move_to_dir(&OUT_DIR.join(module_filename), &target_module_dir)?;
			add_manual(
				&mut BufWriter::new(OpenOptions::new().append(true).open(target_file)?),
				module,
			)?;

			// merge multiple *-.type.rs files into a single types.rs
			let mut header_written = false;
			let mut type_files = files_with_extension(&OUT_DIR, "rs")?
				.filter(|f| is_type_file(f, module))
				.collect::<Vec<_>>();
			type_files.sort_unstable();
			for entry in type_files {
				if entry.metadata().map(|meta| meta.len()).unwrap_or(0) > 0 {
					if !header_written {
						write_has_module(&mut types_rs, module)?;
						writeln!(types_rs, "mod {module}_types {{")?;
						writeln!(types_rs, "\tuse crate::{{mod_prelude::*, core, types, sys}};")?;
						writeln!(types_rs)?;
						header_written = true;
					}
					copy_indent(BufReader::new(File::open(&entry)?), &mut types_rs, "\t")?;
				}
				let _ = fs::remove_file(entry);
			}
			if header_written {
				writeln!(types_rs, "}}")?;
				write_has_module(&mut types_rs, module)?;
				writeln!(types_rs, "pub use {module}_types::*;")?;
				writeln!(types_rs)?;
			}

			// merge module-specific *.externs.rs into a single sys.rs
			let externs_rs = OUT_DIR.join(format!("{module}.externs.rs"));
			write_has_module(&mut sys_rs, module)?;
			writeln!(sys_rs, "mod {module}_sys {{")?;
			writeln!(sys_rs, "\tuse super::*;")?;
			writeln!(sys_rs)?;
			copy_indent(BufReader::new(File::open(&externs_rs)?), &mut sys_rs, "\t")?;
			let _ = fs::remove_file(externs_rs);
			writeln!(sys_rs, "}}")?;
			write_has_module(&mut sys_rs, module)?;
			writeln!(sys_rs, "pub use {module}_sys::*;")?;
			writeln!(sys_rs)?;
		}
		write_module_path(&mut hub_rs, &target_module_dir, "types")?;
		writeln!(hub_rs, "pub mod types;")?;
		write_module_path(&mut hub_rs, &target_module_dir, "sys")?;
		writeln!(hub_rs, "#[doc(hidden)]")?;
		writeln!(hub_rs, "pub mod sys;")?;

		add_manual(&mut types_rs, "types")?;

		add_manual(&mut sys_rs, "sys")?;

		// write hub_prelude that imports all module-specific preludes
		writeln!(hub_rs, "pub mod hub_prelude {{")?;
		for module in modules {
			write!(hub_rs, "\t")?;
			write_has_module(&mut hub_rs, module)?;
			writeln!(hub_rs, "\tpub use super::{module}::prelude::*;")?;
		}
		writeln!(hub_rs, "}}")?;
	}

	if let Some(target_docs_dir) = target_docs_dir {
		transfer_bindings_to_docs(&OUT_DIR, &target_docs_dir);
	}

	Ok(())
}
