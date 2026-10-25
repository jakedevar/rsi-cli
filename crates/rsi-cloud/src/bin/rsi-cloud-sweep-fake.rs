//! Portable classifier fixture for the sweep artifact suite. Never installed.
use std::{env, fs, process};
fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    assert_eq!(args.len(), 5);
    assert_eq!(
        (&*args[0], &*args[1], &*args[3]),
        ("classify", "--log", "--snapshot")
    );
    let log = fs::read_to_string(&args[2]).unwrap();
    let known = fs::read_to_string(&args[4]).unwrap();
    if known == "FAIL_CLASSIFIER" {
        process::exit(19);
    }
    for line in log.lines() {
        if let Some((_, name)) = line.split_once(") test ") {
            if known.lines().any(|k| k == name) {
                println!("KNOWN #7 flake {name}");
            } else {
                println!("NEW {name}");
            }
        }
    }
}
