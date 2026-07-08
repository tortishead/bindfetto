//! `bindfetto-decode` — the CLI adapter over the decode core.
//!
//! Reads bindfetto log lines (from files or stdin) and writes them with transaction
//! codes resolved to method names against an AIDL catalog. A stdin→stdout filter, so
//! it drops straight into a pipeline:
//!
//! ```sh
//! adb logcat -s bindfetto | bindfetto-decode --catalog catalog.json
//! ```

use std::fs;
use std::io::{self, BufRead, BufWriter, Write};
use std::process::ExitCode;

use bindfetto_decode::Decoder;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut catalog_path: Option<&str> = None;
    let mut files: Vec<&str> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--catalog" | "-c" => {
                i += 1;
                match args.get(i) {
                    Some(p) => catalog_path = Some(p),
                    None => {
                        eprintln!("error: --catalog needs a path");
                        return ExitCode::from(2);
                    }
                }
            }
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            flag if flag.starts_with('-') && flag != "-" => {
                eprintln!("error: unknown flag: {flag}");
                return ExitCode::from(2);
            }
            file => files.push(file),
        }
        i += 1;
    }

    let Some(catalog_path) = catalog_path else {
        eprintln!("error: --catalog <catalog.json> is required\n");
        print_usage();
        return ExitCode::from(2);
    };

    let decoder = match load_decoder(catalog_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let result = if files.is_empty() {
        decode_reader(io::stdin().lock(), &decoder, &mut out)
    } else {
        files.iter().try_for_each(|path| {
            let file = fs::File::open(path)
                .map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
            decode_reader(io::BufReader::new(file), &decoder, &mut out)
        })
    };

    match result.and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        // A closed downstream (e.g. `| head`) is a clean stop, not an error.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn load_decoder(path: &str) -> Result<Decoder, String> {
    let json = fs::read_to_string(path).map_err(|e| format!("reading catalog {path}: {e}"))?;
    Decoder::from_catalog_json(&json).map_err(|e| format!("parsing catalog {path}: {e}"))
}

fn decode_reader(reader: impl BufRead, decoder: &Decoder, out: &mut impl Write) -> io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        writeln!(out, "{}", decoder.decode_line(&line))?;
    }
    Ok(())
}

fn print_usage() {
    eprintln!(
        "Usage: bindfetto-decode --catalog <catalog.json> [FILE...]\n\
         \n\
         Resolves bindfetto transaction codes to method names using an AIDL catalog.\n\
         Reads FILEs (or stdin when none are given) line by line and writes the\n\
         decoded lines to stdout. Non-bindfetto lines pass through unchanged.\n\
         \n\
         Options:\n\
         \x20 -c, --catalog <path>   AIDL catalog JSON (required)\n\
         \x20 -h, --help             show this help\n\
         \n\
         Example:\n\
         \x20 adb logcat -s bindfetto | bindfetto-decode -c catalog.json"
    );
}
