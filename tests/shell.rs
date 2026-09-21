//! The two halves of augur must agree: `shell/augur.bash` is checked here
//! against the engine it talks to.

use std::process::Command;

use augur::escape::escape;
use augur::protocol::VERSION;

const SCRIPT: &str = include_str!("../shell/augur.bash");

/// The source of a function of the script, which runs without ble.sh.
fn function(name: &str) -> &'static str {
    let start = SCRIPT
        .find(&format!("function {name} {{"))
        .unwrap_or_else(|| panic!("{name} is not in the script"));
    let end = SCRIPT[start..].find("\n}\n").expect("the function's end");
    &SCRIPT[start..start + end + 3]
}

#[test]
fn speaks_the_protocol_of_the_engine() {
    let stated = SCRIPT
        .lines()
        .find_map(|line| line.strip_prefix("_ble_augur_protocol="))
        .expect("the script states its protocol");
    assert_eq!(stated, VERSION.to_string());
}

#[test]
fn escapes_as_the_engine_does() {
    let texts = [
        "plain",
        "tab\there",
        "two\nlines\r",
        r"back\slash and \t literal",
        "a & b \\& c",
        "",
    ];
    for text in texts {
        let output = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "{}\nble/augur/.escape \"$1\"; printf %s \"$ret\"",
                function("ble/augur/.escape")
            ))
            .arg("bash")
            .arg(text)
            .output()
            .expect("run bash");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            escape(text),
            "{text:?}"
        );
    }
}
