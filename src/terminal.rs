use std::io::{self, Write};

pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
pub const RESET: &str = "\x1b[0m";

pub fn title(message: &str) {
    println!("{YELLOW}{message}{RESET}");
}

pub fn separator(message: &str) {
    println!("{YELLOW}{message}{RESET}");
}

pub fn menu_item(number: &str, message: &str) {
    println!("{YELLOW}{number}{RESET}. {CYAN}{message}{RESET}");
}

pub fn command_item(command: &str, description: &str) {
    println!("  {CYAN}{command:<27}{RESET} {description}");
}

pub fn info(message: &str) {
    println!("{CYAN}{message}{RESET}");
}

pub fn success(message: &str) {
    println!("{GREEN}{message}{RESET}");
}

pub fn error(message: &str) {
    println!("{RED}{message}{RESET}");
}

pub fn warning(message: &str) {
    println!("{YELLOW}{message}{RESET}");
}

/// Prints a cyan prompt, reads one line, then adds a blank line so the
/// following response is visually separated from the user's input.
pub fn read_input(prompt: &str) -> io::Result<String> {
    print!("{CYAN}{prompt}{RESET}");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    // Keep one fully empty line between the entered value and the next response.
    println!();
    println!();

    Ok(input.trim().to_owned())
}
