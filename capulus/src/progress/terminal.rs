use std::io;

use console::Term;
use indicatif::{ProgressDrawTarget, TermLike};

pub(super) fn draw_target() -> ProgressDrawTarget {
    ProgressDrawTarget::term_like_with_hz(Box::new(ProgressTerminal(Term::buffered_stderr())), 20)
}

#[derive(Debug)]
struct ProgressTerminal(Term);

impl TermLike for ProgressTerminal {
    fn width(&self) -> u16 {
        self.0.size().1
    }

    fn height(&self) -> u16 {
        self.0.size().0
    }

    fn move_cursor_up(&self, lines: usize) -> io::Result<()> {
        self.0.move_cursor_up(lines)
    }

    fn move_cursor_down(&self, lines: usize) -> io::Result<()> {
        self.0.move_cursor_down(lines)
    }

    fn move_cursor_right(&self, columns: usize) -> io::Result<()> {
        self.0.move_cursor_right(columns)
    }

    fn move_cursor_left(&self, columns: usize) -> io::Result<()> {
        self.0.move_cursor_left(columns)
    }

    fn write_line(&self, line: &str) -> io::Result<()> {
        self.0.write_line(line)
    }

    fn write_str(&self, text: &str) -> io::Result<()> {
        self.0.write_str(text)
    }

    fn clear_line(&self) -> io::Result<()> {
        // Replace the row, including its soft-wrap boundary, while preserving the rows below it.
        // Erasing characters alone can join later permanent output to discarded progress on reflow.
        self.0.write_str("\r\x1b[M\x1b[L")
    }

    fn flush(&self) -> io::Result<()> {
        self.0.flush()
    }
}
