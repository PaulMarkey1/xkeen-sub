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
    println!("{YELLOW}{number}{RESET}. {message}");
}

pub fn command_item(command: &str, description: &str) {
    println!("  {CYAN}{command:<27}{RESET} {description}");
}

pub fn info(message: &str) {
    println!("{message}");
}

/// Prints an object name in cyan and its explanation in the terminal's normal color.
pub fn info_prefix(prefix: &str, message: &str) {
    println!("{CYAN}{prefix}{RESET} — {message}");
}

/// A determinate progress bar for file operations.
pub struct ProgressBar {
    total: usize,
    current: usize,
    active: bool,
}

impl ProgressBar {
    pub fn new(total: usize) -> Self {
        let active = total > 0;
        let progress = Self { total, current: 0, active };
        if active {
            progress.render("Подготовка");
        }
        progress
    }

    pub fn advance(&mut self, message: &str) {
        if !self.active {
            return;
        }
        self.current = (self.current + 1).min(self.total);
        self.render(message);
    }

    pub fn finish(&mut self, message: &str) {
        if !self.active {
            return;
        }
        self.current = self.total;
        self.render(message);
        println!();
        self.active = false;
    }

    fn render(&self, message: &str) {
        let width = 24usize;
        let filled = width * self.current / self.total;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(width - filled));
        let percent = self.current * 100 / self.total;
        print!("\r{CYAN}[{bar}]{RESET} {percent:>3}% ({}/{}) {message}\x1b[K", self.current, self.total);
        let _ = io::stdout().flush();
    }
}

impl Drop for ProgressBar {
    fn drop(&mut self) {
        if self.active {
            println!();
        }
    }
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
