//! Fernet cipher CLI — the Rust twin of the crypto bits of `aimap accounts`.
//!
//!     cipher decrypt <keys-csv> <user> <token>     # prints the password
//!     cipher encrypt <keys-csv> <user> <password>  # prints a token
//!     cipher rotate <keys-csv> <token>             # re-encrypts with the first key
//!     cipher keygen                                # prints a fresh Fernet key
//!
//! Tokens are wire-compatible with Python's `cryptography.fernet`.

use aimap::crypto::{generate_key, Cipher};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args.first().map(String::as_str) {
        Some("keygen") if args.len() == 1 => Ok(generate_key()),
        Some("encrypt") if args.len() == 4 => Cipher::new(&args[1])
            .map(|c| c.encrypt(&args[2], &args[3]))
            .map_err(|e| e.to_string()),
        Some("decrypt") if args.len() == 4 => Cipher::new(&args[1])
            .and_then(|c| c.decrypt(&args[2], &args[3]))
            .map_err(|e| e.to_string()),
        Some("rotate") if args.len() == 3 => Cipher::new(&args[1])
            .and_then(|c| c.rotate(&args[2]))
            .map_err(|e| e.to_string()),
        _ => Err("usage: cipher <keygen|encrypt|decrypt|rotate> ...".to_string()),
    };
    match outcome {
        Ok(out) => println!("{out}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
