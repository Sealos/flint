//! The scripted shell behind the fake exec endpoint ([`super::exec`]): deterministic, with no
//! PTY, but byte for byte what a client sees from bash (Debian's nginx image) and busybox `sh`
//! (Alpine's podinfo image) for what Flint does. Recorded on the kind dev cluster (Kubernetes
//! v1.37, containerd):
//!
//! - **Prompt.** bash turns bracketed paste on with every prompt: `\e[?2004h` then
//!   `root@<pod>:/# ` (the pod's name is its hostname). busybox prints `~ $ ` (podinfo's user,
//!   recorded) or `/ # ` as root, then asks for the cursor position (`\e[6n`); the terminal's
//!   answer comes back on stdin.
//! - **Line editing** (the TTY echoes): typed bytes are echoed; Backspace (`\x7f`) erases one
//!   character as `\b \b`; Enter (`\r`) echoes `\r\n`, and bash then turns bracketed paste off
//!   (`\e[?2004l\r`) before the command's output. Escape sequences (arrows, the terminal's
//!   answers) are swallowed.
//! - **Ctrl-C** (`\x03`) drops the line, prints `^C` and a fresh prompt, and stops a running
//!   program (bash's `$?` becomes 130). **Ctrl-D** (`\x04`) on an empty line ends the shell with
//!   the last status (bash prints `exit`); on a non-empty line bash rings the bell.
//! - **Bracketed paste** (bash only): a paste between `\e[200~` and `\e[201~` is shown in
//!   reverse video and inserted into the line, newlines included; Enter then runs its lines
//!   in order. (bash also redraws the prompt there; the fake does not.)
//! - **Resize:** busybox redraws its prompt on every SIGWINCH (`\r~ $ \e[J`), bash only when
//!   the size differs from what readline last knew (80x24 until the first resize):
//!   `\r\e[K\r<prompt>`. `stty size` prints `<rows> <cols>`; before any resize the fake says
//!   `0 0` (not recorded: Flint always resizes first).
//! - **Commands:** `echo` (with `-n`, quotes, `$?` and `>&2`), `exit [N]`, `true`, `false`,
//!   `stty size`, `seq [A] B` and `yes [text]` (which floods until Ctrl-C), separated by `;`
//!   or newlines. Anything else is `bash: X: command not found` / `sh: X: not found` with 127,
//!   naming the shell as it was invoked (`/bin/sh: X: not found`).
//! - **Without a TTY** nothing is echoed and there is no prompt; lines end at `\n`, errors go to
//!   stderr, and closing stdin ends the shell with the last status. With a TTY, closing stdin
//!   changes nothing: the shell keeps running (bash on kind did).
//!
//! Output uses `\r\n` on a TTY (`onlcr`) and `\n` without one.

use std::collections::VecDeque;

/// Which shell runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Bash,
    /// busybox `sh` (`ash`).
    Sh,
}

/// What an exec runs, from its `command` parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Program {
    /// An interactive shell, named as invoked (`bash`, `/bin/sh`).
    Shell { flavor: Flavor, argv0: String },
    /// `<shell> -c <script>`.
    Script {
        flavor: Flavor,
        argv0: String,
        script: String,
    },
    /// A utility on its own (`echo hi`, `seq 3`, `yes`).
    Utility(Vec<String>),
}

/// The utilities every container has, besides its shells.
const UTILITIES: &[&str] = &["echo", "true", "false", "seq", "yes", "stty"];

impl Program {
    /// What `command` runs; `None` when no such executable is known (a path counts by its last
    /// component). Whether the container has it is for [`super::exec`] to say.
    pub fn parse(command: &[String]) -> Option<Program> {
        let argv0 = command.first()?;
        let name = argv0.rsplit('/').next().unwrap_or(argv0);
        let flavor = match name {
            "bash" => Some(Flavor::Bash),
            "sh" | "ash" => Some(Flavor::Sh),
            _ => None,
        };
        match (flavor, command.get(1).map(String::as_str)) {
            (Some(flavor), Some("-c")) => Some(Program::Script {
                flavor,
                argv0: argv0.clone(),
                script: command.get(2).cloned().unwrap_or_default(),
            }),
            (Some(flavor), _) => Some(Program::Shell {
                flavor,
                argv0: argv0.clone(),
            }),
            (None, _) if UTILITIES.contains(&name) => {
                let mut argv = command.to_vec();
                argv[0] = name.to_owned();
                Some(Program::Utility(argv))
            }
            (None, _) => None,
        }
    }

    /// The shell this program needs, if it is one.
    pub fn flavor(&self) -> Option<Flavor> {
        match self {
            Program::Shell { flavor, .. } | Program::Script { flavor, .. } => Some(*flavor),
            Program::Utility(_) => None,
        }
    }
}

/// What the shell does, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The process ended with this status; nothing follows.
    Exit(i32),
}

/// A TTY size, as the resize channel sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TtySize {
    pub width: u16,
    pub height: u16,
}

/// Who the shell runs as and where, for its prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Host {
    /// The pod's name.
    pub hostname: String,
    pub root: bool,
}

/// A program that floods its output.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Flood {
    /// `seq`: the next number and the last.
    Seq { next: u64, last: u64 },
    /// `yes`: one line, forever.
    Yes(Vec<u8>),
}

/// How much a flood sends at a time.
const FLOOD_CHUNK: usize = 16 * 1024;

/// The readline's idea of the screen before the first resize.
const READLINE_DEFAULT: TtySize = TtySize {
    width: 80,
    height: 24,
};

/// Where an escape sequence in the input stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Escape {
    None,
    /// After `ESC`.
    Started,
    /// After `ESC O`: one more byte.
    Ss3,
    /// Inside `ESC [`, with the parameters so far.
    Csi(Vec<u8>),
}

/// A running scripted program: feed it stdin, resizes and time ([`Shell::pump`]); it answers
/// with [`Event`]s.
#[derive(Debug)]
pub(crate) struct Shell {
    flavor: Flavor,
    argv0: String,
    host: Host,
    tty: bool,
    /// Reads commands from stdin (an interactive shell; not `-c` or a utility).
    interactive: bool,
    line: Vec<u8>,
    escape: Escape,
    /// Inside a bracketed paste: what was pasted so far.
    paste: Option<Vec<u8>>,
    /// The TTY's size (`0 0` until the first resize).
    size: TtySize,
    /// bash's idea of the size, to redraw only when it changes.
    readline: TtySize,
    /// `$?`
    status: i32,
    /// Commands still to run, each split into words.
    queue: VecDeque<Vec<String>>,
    flood: Option<Flood>,
    /// The process ended.
    done: bool,
}

impl Shell {
    /// Starts `program`; returns it with what it prints first (a prompt, or a whole script's
    /// output when nothing in it floods).
    pub fn start(program: Program, host: Host, tty: bool) -> (Shell, Vec<Event>) {
        let (flavor, argv0, interactive) = match &program {
            Program::Shell { flavor, argv0 } => (*flavor, argv0.clone(), true),
            Program::Script { flavor, argv0, .. } => (*flavor, argv0.clone(), false),
            Program::Utility(argv) => (Flavor::Sh, argv[0].clone(), false),
        };
        let mut shell = Shell {
            flavor,
            argv0,
            host,
            tty,
            interactive,
            line: Vec::new(),
            escape: Escape::None,
            paste: None,
            size: TtySize::default(),
            readline: READLINE_DEFAULT,
            status: 0,
            queue: VecDeque::new(),
            flood: None,
            done: false,
        };
        let mut events = Vec::new();
        match program {
            Program::Shell { .. } => {
                if tty {
                    events.push(Event::Stdout(shell.prompt()));
                }
            }
            Program::Script { script, .. } => {
                shell.queue.extend(split_commands(&script));
                shell.run_queue(&mut events);
            }
            Program::Utility(argv) => {
                shell.queue.push_back(argv);
                shell.run_queue(&mut events);
            }
        }
        (shell, events)
    }

    /// Whether the process ended.
    #[cfg(test)]
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Whether a program floods output, so [`Shell::pump`] has more to send.
    pub fn is_flooding(&self) -> bool {
        self.flood.is_some() && !self.done
    }

    /// The prompt, bracketed paste and all.
    fn prompt(&self) -> Vec<u8> {
        match (self.flavor, self.host.root) {
            (Flavor::Bash, true) => format!("\x1b[?2004hroot@{}:/# ", self.host.hostname),
            (Flavor::Bash, false) => "\x1b[?2004h$ ".to_owned(),
            (Flavor::Sh, true) => "/ # \x1b[6n".to_owned(),
            (Flavor::Sh, false) => "~ $ \x1b[6n".to_owned(),
        }
        .into_bytes()
    }

    /// The prompt as redrawn after a resize.
    fn redraw(&self) -> Vec<u8> {
        let mut out = match (self.flavor, self.host.root) {
            (Flavor::Bash, true) => format!("\r\x1b[K\rroot@{}:/# ", self.host.hostname),
            (Flavor::Bash, false) => "\r\x1b[K\r$ ".to_owned(),
            (Flavor::Sh, true) => "\r/ # ".to_owned(),
            (Flavor::Sh, false) => "\r~ $ ".to_owned(),
        }
        .into_bytes();
        out.extend_from_slice(&self.line);
        if self.flavor == Flavor::Sh {
            out.extend_from_slice(b"\x1b[J");
        }
        out
    }

    /// Text as the TTY prints it (`\n` as `\r\n`).
    fn text(&self, text: &str) -> Vec<u8> {
        if self.tty {
            text.replace('\n', "\r\n").into_bytes()
        } else {
            text.as_bytes().to_vec()
        }
    }

    /// An error message: on stderr, or on the TTY when there is one.
    fn error(&self, text: &str) -> Event {
        if self.tty {
            Event::Stdout(self.text(text))
        } else {
            Event::Stderr(self.text(text))
        }
    }

    /// The TTY was resized.
    pub fn resize(&mut self, size: TtySize) -> Vec<Event> {
        let changed = size != self.size;
        self.size = size;
        if !changed || !self.tty || !self.at_prompt() {
            return Vec::new();
        }
        let redraw = match self.flavor {
            Flavor::Sh => true,
            Flavor::Bash => size != self.readline,
        };
        self.readline = size;
        if redraw {
            vec![Event::Stdout(self.redraw())]
        } else {
            Vec::new()
        }
    }

    /// Waiting for a command at its prompt.
    fn at_prompt(&self) -> bool {
        self.interactive && !self.done && self.flood.is_none() && self.queue.is_empty()
    }

    /// The client closed stdin (`v5.channel.k8s.io`'s close signal).
    pub fn close_stdin(&mut self) -> Vec<Event> {
        if self.tty || !self.interactive || self.done {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.line.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
            self.queue.extend(split_commands(&line));
            self.run_queue(&mut events);
        }
        if !self.done {
            self.finish(&mut events);
        }
        events
    }

    fn finish(&mut self, events: &mut Vec<Event>) {
        self.done = true;
        self.flood = None;
        events.push(Event::Exit(self.status));
    }

    /// Bytes on stdin.
    pub fn input(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.done {
                break;
            }
            self.byte(byte, &mut events);
        }
        merge(events)
    }

    fn byte(&mut self, byte: u8, events: &mut Vec<Event>) {
        if self.tty && self.escape(byte, events) {
            return;
        }
        if let Some(paste) = &mut self.paste {
            paste.push(byte);
            return;
        }
        if self.flood.is_some() || !self.interactive {
            // A running program reads no input; only the TTY's Ctrl-C reaches it.
            if self.tty && byte == 0x03 {
                self.interrupt(events);
            }
            return;
        }
        if !self.tty {
            if byte == b'\n' {
                let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
                self.queue.extend(split_commands(&line));
                self.run_queue(events);
            } else {
                self.line.push(byte);
            }
            return;
        }
        match byte {
            b'\r' | b'\n' => self.enter(events),
            0x7f | 0x08 => {
                if pop_char(&mut self.line) {
                    events.push(Event::Stdout(b"\x08 \x08".to_vec()));
                }
            }
            0x03 => self.interrupt(events),
            0x04 if self.line.is_empty() => {
                if self.flavor == Flavor::Bash {
                    events.push(Event::Stdout(b"\x1b[?2004l\r\r\nexit\r\n".to_vec()));
                } else {
                    events.push(Event::Stdout(b"\r\n".to_vec()));
                }
                self.finish(events);
            }
            0x04 => {
                if self.flavor == Flavor::Bash {
                    events.push(Event::Stdout(vec![0x07]));
                }
            }
            b if b < 0x20 => {}
            b => {
                self.line.push(b);
                events.push(Event::Stdout(vec![b]));
            }
        }
    }

    /// Follows escape sequences in TTY input; returns whether `byte` was part of one.
    fn escape(&mut self, byte: u8, events: &mut Vec<Event>) -> bool {
        match &mut self.escape {
            Escape::None if byte == 0x1b => {
                self.escape = Escape::Started;
                true
            }
            Escape::None => false,
            Escape::Started => {
                self.escape = match byte {
                    b'[' => Escape::Csi(Vec::new()),
                    b'O' => Escape::Ss3,
                    _ => Escape::None,
                };
                true
            }
            Escape::Ss3 => {
                self.escape = Escape::None;
                true
            }
            Escape::Csi(params) => {
                if !(0x40..=0x7e).contains(&byte) {
                    params.push(byte);
                    return true;
                }
                let sequence = std::mem::take(params);
                self.escape = Escape::None;
                if byte == b'~' && self.flavor == Flavor::Bash && self.interactive {
                    match sequence.as_slice() {
                        b"200" => self.paste = Some(Vec::new()),
                        b"201" => self.end_paste(events),
                        _ => {}
                    }
                }
                true
            }
        }
    }

    /// A bracketed paste ended: its text joins the line, shown in reverse video.
    fn end_paste(&mut self, events: &mut Vec<Event>) {
        let Some(paste) = self.paste.take() else {
            return;
        };
        let text: Vec<u8> = paste
            .iter()
            .map(|&b| if b == b'\r' { b'\n' } else { b })
            .collect();
        let mut shown = b"\x1b[7m".to_vec();
        for &b in &text {
            if b == b'\n' {
                shown.extend_from_slice(b"\x1b[27m\r\n\r\x1b[7m");
            } else {
                shown.push(b);
            }
        }
        shown.extend_from_slice(b"\x1b[27m");
        self.line.extend_from_slice(&text);
        events.push(Event::Stdout(shown));
    }

    fn enter(&mut self, events: &mut Vec<Event>) {
        let mut echo = b"\r\n".to_vec();
        if self.flavor == Flavor::Bash {
            echo.extend_from_slice(b"\x1b[?2004l\r");
        }
        events.push(Event::Stdout(echo));
        let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
        self.queue.extend(split_commands(&line));
        self.run_queue(events);
    }

    /// Ctrl-C on the TTY: the line or the running program is dropped.
    fn interrupt(&mut self, events: &mut Vec<Event>) {
        self.line.clear();
        self.queue.clear();
        let was_running = self.flood.take().is_some();
        if !self.interactive {
            // The program itself gets SIGINT and dies of it.
            events.push(Event::Stdout(b"^C".to_vec()));
            self.status = 130;
            self.finish(events);
            return;
        }
        let echo: &[u8] = match (self.flavor, was_running) {
            (Flavor::Bash, false) => b"^C\x1b[?2004l\r\r\n",
            (Flavor::Sh, false) => b"^C\r\n\r\n",
            (_, true) => b"^C\r\n",
        };
        events.push(Event::Stdout(echo.to_vec()));
        if self.flavor == Flavor::Bash || was_running {
            self.status = 130;
        }
        events.push(Event::Stdout(self.prompt()));
    }

    /// Runs queued commands until one floods or the shell exits; then prompts (or, for a script
    /// or a utility, exits).
    fn run_queue(&mut self, events: &mut Vec<Event>) {
        while self.flood.is_none() && !self.done {
            let Some(words) = self.queue.pop_front() else {
                break;
            };
            self.run(words, events);
        }
        if self.flood.is_some() || self.done {
            return;
        }
        if self.interactive {
            if self.tty {
                events.push(Event::Stdout(self.prompt()));
            }
        } else {
            self.finish(events);
        }
    }

    /// Runs one command.
    fn run(&mut self, words: Vec<String>, events: &mut Vec<Event>) {
        let (mut words, to_stderr): (Vec<String>, bool) = {
            let to_stderr = words.iter().any(|w| w == ">&2" || w == "1>&2");
            (
                words
                    .into_iter()
                    .filter(|w| w != ">&2" && w != "1>&2")
                    .collect(),
                to_stderr,
            )
        };
        for word in &mut words {
            *word = word.replace("$?", &self.status.to_string());
        }
        let Some(name) = words.first().cloned() else {
            return;
        };
        let args = &words[1..];
        let print = |shell: &Shell, text: &str| {
            if to_stderr {
                shell.error(text)
            } else {
                Event::Stdout(shell.text(text))
            }
        };
        self.status = match name.as_str() {
            "echo" => {
                let (newline, args) = match args.first().map(String::as_str) {
                    Some("-n") => (false, &args[1..]),
                    _ => (true, args),
                };
                let mut text = args.join(" ");
                if newline {
                    text.push('\n');
                }
                if !text.is_empty() {
                    events.push(print(self, &text));
                }
                0
            }
            "true" => 0,
            "false" => 1,
            "exit" => {
                let code = match args.first() {
                    None => self.status,
                    Some(n) => n.parse::<i64>().map_or(2, |n| (n & 0xff) as i32),
                };
                if self.flavor == Flavor::Bash && self.interactive && self.tty {
                    events.push(Event::Stdout(b"exit\r\n".to_vec()));
                }
                self.status = code;
                self.finish(events);
                return;
            }
            "stty" => match args.first().map(String::as_str) {
                _ if !self.tty => {
                    events
                        .push(self.error("stty: standard input: Inappropriate ioctl for device\n"));
                    1
                }
                Some("size") => {
                    let size = format!("{} {}\n", self.size.height, self.size.width);
                    events.push(print(self, &size));
                    0
                }
                _ => 0,
            },
            "seq" => {
                let numbers: Option<Vec<u64>> = args.iter().map(|a| a.parse().ok()).collect();
                match numbers.as_deref() {
                    Some([last]) => {
                        self.flood = Some(Flood::Seq {
                            next: 1,
                            last: *last,
                        })
                    }
                    Some([first, last]) => {
                        self.flood = Some(Flood::Seq {
                            next: *first,
                            last: *last,
                        });
                    }
                    _ => {
                        events.push(self.error("seq: invalid arguments\n"));
                        self.status = 1;
                        return;
                    }
                }
                0
            }
            "yes" => {
                let text = if args.is_empty() {
                    "y".to_owned()
                } else {
                    args.join(" ")
                };
                self.flood = Some(Flood::Yes(self.text(&format!("{text}\n"))));
                0
            }
            other => {
                let message = match self.flavor {
                    Flavor::Bash => format!("{}: {other}: command not found\n", self.argv0),
                    Flavor::Sh => format!("{}: {other}: not found\n", self.argv0),
                };
                events.push(self.error(&message));
                127
            }
        };
    }

    /// The next part of a flooding program's output; when it ends, what follows it (the next
    /// commands, a prompt or the exit).
    pub fn pump(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let Some(flood) = &mut self.flood else {
            return events;
        };
        let mut chunk = Vec::with_capacity(FLOOD_CHUNK + 32);
        let mut ended = false;
        match flood {
            Flood::Seq { next, last } => {
                while chunk.len() < FLOOD_CHUNK {
                    if *next > *last {
                        ended = true;
                        break;
                    }
                    chunk.extend_from_slice(next.to_string().as_bytes());
                    chunk.extend_from_slice(if self.tty { b"\r\n" } else { b"\n" });
                    *next += 1;
                }
            }
            Flood::Yes(line) => {
                while chunk.len() < FLOOD_CHUNK {
                    chunk.extend_from_slice(line);
                }
            }
        }
        if !chunk.is_empty() {
            events.push(Event::Stdout(chunk));
        }
        if ended {
            self.flood = None;
            self.run_queue(&mut events);
        }
        events
    }
}

/// Removes the last UTF-8 character; returns whether there was one.
fn pop_char(line: &mut Vec<u8>) -> bool {
    let Some(last) = line.iter().rposition(|b| b & 0xc0 != 0x80) else {
        return false;
    };
    line.truncate(last);
    true
}

/// Joins consecutive output events, as one write would carry them.
fn merge(events: Vec<Event>) -> Vec<Event> {
    let mut merged: Vec<Event> = Vec::with_capacity(events.len());
    for event in events {
        match (merged.last_mut(), event) {
            (Some(Event::Stdout(a)), Event::Stdout(b))
            | (Some(Event::Stderr(a)), Event::Stderr(b)) => {
                a.extend_from_slice(&b);
            }
            (_, event) => merged.push(event),
        }
    }
    merged
}

/// Splits a script into commands (at `;` and newlines outside quotes), each into words (with
/// single and double quotes removed).
fn split_commands(script: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in script.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, ';' | '\n') => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if !words.is_empty() {
        commands.push(words);
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            hostname: "web-1".into(),
            root: true,
        }
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn bash() -> Shell {
        let program = Program::parse(&argv(&["bash"])).unwrap();
        let (shell, events) = Shell::start(program, host(), true);
        assert_eq!(
            events,
            [Event::Stdout(b"\x1b[?2004hroot@web-1:/# ".to_vec())]
        );
        shell
    }

    fn sh(tty: bool) -> (Shell, Vec<Event>) {
        let program = Program::parse(&argv(&["/bin/sh"])).unwrap();
        let host = Host {
            hostname: "podinfo".into(),
            root: false,
        };
        Shell::start(program, host, tty)
    }

    fn out(text: &str) -> Vec<Event> {
        vec![Event::Stdout(text.as_bytes().to_vec())]
    }

    #[test]
    fn programs_are_parsed_from_the_command() {
        assert_eq!(
            Program::parse(&argv(&["bash"])),
            Some(Program::Shell {
                flavor: Flavor::Bash,
                argv0: "bash".into()
            })
        );
        assert_eq!(
            Program::parse(&argv(&["/bin/sh", "-c", "exit 3"])),
            Some(Program::Script {
                flavor: Flavor::Sh,
                argv0: "/bin/sh".into(),
                script: "exit 3".into()
            })
        );
        assert_eq!(
            Program::parse(&argv(&["/usr/bin/seq", "3"])),
            Some(Program::Utility(argv(&["seq", "3"])))
        );
        assert_eq!(Program::parse(&argv(&["zsh"])), None);
        assert_eq!(Program::parse(&[]), None);
        assert_eq!(
            Program::parse(&argv(&["ash"])).unwrap().flavor(),
            Some(Flavor::Sh)
        );
        assert_eq!(Program::parse(&argv(&["yes"])).unwrap().flavor(), None);
    }

    #[test]
    fn bash_echoes_runs_commands_and_prompts_as_recorded() {
        let mut shell = bash();
        assert_eq!(shell.input(b"echo hi"), out("echo hi"));
        assert_eq!(
            shell.input(b"\r"),
            out("\r\n\x1b[?2004l\rhi\r\n\x1b[?2004hroot@web-1:/# ")
        );
        assert_eq!(
            shell.input(b"echo ab\x7fc\r"),
            out("echo ab\x08 \x08c\r\n\x1b[?2004l\rac\r\n\x1b[?2004hroot@web-1:/# ")
        );
        shell.input(b"false\r");
        assert_eq!(
            shell.input(b"echo $?\r"),
            out("echo $?\r\n\x1b[?2004l\r1\r\n\x1b[?2004hroot@web-1:/# ")
        );
        assert_eq!(
            shell.input(b"nosuch\r"),
            out(
                "nosuch\r\n\x1b[?2004l\rbash: nosuch: command not found\r\n\x1b[?2004hroot@web-1:/# "
            )
        );
        assert_eq!(
            shell.input(b"exit 3\r"),
            vec![
                Event::Stdout(b"exit 3\r\n\x1b[?2004l\rexit\r\n".to_vec()),
                Event::Exit(3)
            ]
        );
        assert!(shell.is_done());
        assert_eq!(shell.input(b"echo late\r"), []);
    }

    #[test]
    fn ctrl_c_and_ctrl_d_follow_bash() {
        let mut shell = bash();
        shell.input(b"echo par");
        assert_eq!(
            shell.input(b"\x03"),
            out("^C\x1b[?2004l\r\r\n\x1b[?2004hroot@web-1:/# ")
        );
        // Ctrl-D on a line rings the bell; Ctrl-C leaves $? at 130, which a bare exit uses.
        shell.input(b"ab");
        assert_eq!(shell.input(b"\x04"), out("\x07"));
        shell.input(b"\x03");
        let events = shell.input(b"exit\r");
        assert_eq!(events.last(), Some(&Event::Exit(130)));

        let mut shell = bash();
        assert_eq!(
            shell.input(b"\x04"),
            vec![
                Event::Stdout(b"\x1b[?2004l\r\r\nexit\r\n".to_vec()),
                Event::Exit(0)
            ]
        );
    }

    #[test]
    fn a_bracketed_paste_waits_for_enter_and_runs_its_lines() {
        let mut shell = bash();
        assert_eq!(
            shell.input(b"\x1b[200~echo a\recho b\x1b[201~"),
            out("\x1b[7mecho a\x1b[27m\r\n\r\x1b[7mecho b\x1b[27m")
        );
        assert_eq!(
            shell.input(b"\r"),
            out("\r\n\x1b[?2004l\ra\r\nb\r\n\x1b[?2004hroot@web-1:/# ")
        );
    }

    #[test]
    fn escape_sequences_are_swallowed() {
        let mut shell = bash();
        assert_eq!(shell.input(b"\x1b[A\x1bOB\x1b[1;1Rx"), out("x"));
        // Split across writes too.
        assert_eq!(shell.input(b"\x1b"), []);
        assert_eq!(shell.input(b"[12;"), []);
        assert_eq!(shell.input(b"3Ry"), out("y"));
    }

    #[test]
    fn sh_prompts_asks_for_the_cursor_and_names_itself_in_errors() {
        let (mut shell, events) = sh(true);
        assert_eq!(events, out("~ $ \x1b[6n"));
        assert_eq!(shell.input(b"\x1b[1;1R"), []);
        assert_eq!(
            shell.input(b"nosuch\r"),
            out("nosuch\r\n/bin/sh: nosuch: not found\r\n~ $ \x1b[6n")
        );
        assert_eq!(shell.input(b"par\x03"), out("par^C\r\n\r\n~ $ \x1b[6n"));
        assert_eq!(
            shell.input(b"exit 4\r"),
            vec![Event::Stdout(b"exit 4\r\n".to_vec()), Event::Exit(4)]
        );
        let (mut shell, _) = sh(true);
        assert_eq!(
            shell.input(b"\x04"),
            vec![Event::Stdout(b"\r\n".to_vec()), Event::Exit(0)]
        );
    }

    #[test]
    fn resizes_redraw_the_prompt_and_show_in_stty_size() {
        let (mut shell, _) = sh(true);
        assert_eq!(
            shell.input(b"stty size\r"),
            out("stty size\r\n0 0\r\n~ $ \x1b[6n")
        );
        let size = TtySize {
            width: 80,
            height: 24,
        };
        assert_eq!(shell.resize(size), out("\r~ $ \x1b[J"));
        assert_eq!(shell.resize(size), []);
        // bash's readline starts at 80x24: only a different size redraws.
        let mut bash = bash();
        assert_eq!(bash.resize(size), []);
        bash.input(b"ls");
        assert_eq!(
            bash.resize(TtySize {
                width: 100,
                height: 30
            }),
            out("\r\x1b[K\rroot@web-1:/# ls")
        );
        bash.input(b"\x03");
        assert_eq!(
            bash.input(b"stty size\r"),
            out("stty size\r\n\x1b[?2004l\r30 100\r\n\x1b[?2004hroot@web-1:/# ")
        );
    }

    #[test]
    fn floods_run_until_done_or_interrupted() {
        let mut shell = bash();
        let events = shell.input(b"seq 20000\r");
        assert_eq!(events, out("seq 20000\r\n\x1b[?2004l\r"));
        assert!(shell.is_flooding());
        let mut printed = Vec::new();
        let mut chunks = 0;
        while shell.is_flooding() {
            for event in shell.pump() {
                if let Event::Stdout(bytes) = event {
                    printed.extend(bytes);
                }
            }
            chunks += 1;
        }
        assert!(chunks > 3, "{chunks} chunks");
        let text = String::from_utf8(printed).unwrap();
        assert!(text.starts_with("1\r\n2\r\n"));
        assert!(text.ends_with("19999\r\n20000\r\n\x1b[?2004hroot@web-1:/# "));

        shell.input(b"yes hello\r");
        let first = shell.pump();
        assert!(matches!(&first[..], [Event::Stdout(b)] if b.starts_with(b"hello\r\nhello\r\n")));
        // Typing does not reach a running program; Ctrl-C stops it.
        assert_eq!(shell.input(b"ls"), []);
        assert_eq!(shell.input(b"\x03"), out("^C\r\n\x1b[?2004hroot@web-1:/# "));
        assert!(!shell.is_flooding());
        assert_eq!(shell.pump(), []);
    }

    #[test]
    fn scripts_and_utilities_run_and_exit() {
        let program = Program::parse(&argv(&["sh", "-c", "echo out; echo err >&2; exit 2"]));
        let (shell, events) = Shell::start(program.unwrap(), host(), false);
        assert_eq!(
            events,
            [
                Event::Stdout(b"out\n".to_vec()),
                Event::Stderr(b"err\n".to_vec()),
                Event::Exit(2)
            ]
        );
        assert!(shell.is_done());
        let program = Program::parse(&argv(&["sh", "-c", "nosuchcmd"]));
        let (_, events) = Shell::start(program.unwrap(), host(), true);
        assert_eq!(
            events,
            [
                Event::Stdout(b"sh: nosuchcmd: not found\r\n".to_vec()),
                Event::Exit(127)
            ]
        );
        let program = Program::parse(&argv(&["echo", "a b", "'c'"]));
        let (_, events) = Shell::start(program.unwrap(), host(), false);
        assert_eq!(
            events,
            [Event::Stdout(b"a b 'c'\n".to_vec()), Event::Exit(0)]
        );
        let (mut yes, events) = Shell::start(Program::Utility(argv(&["yes"])), host(), true);
        assert_eq!(events, []);
        assert!(yes.is_flooding());
        assert_eq!(
            yes.input(b"\x03"),
            vec![Event::Stdout(b"^C".to_vec()), Event::Exit(130)]
        );
        let (_, events) = Shell::start(Program::Utility(argv(&["seq", "2", "3"])), host(), false);
        assert!(events.is_empty());
    }

    #[test]
    fn without_a_tty_lines_run_silently_until_stdin_closes() {
        let (mut shell, events) = sh(false);
        assert_eq!(events, []);
        assert_eq!(
            shell.input(b"echo one\nnosuch\n"),
            vec![
                Event::Stdout(b"one\n".to_vec()),
                Event::Stderr(b"/bin/sh: nosuch: not found\n".to_vec())
            ]
        );
        assert_eq!(
            shell.input(b"stty size\n"),
            vec![Event::Stderr(
                b"stty: standard input: Inappropriate ioctl for device\n".to_vec()
            )]
        );
        assert_eq!(shell.close_stdin(), [Event::Exit(1)]);
        // With a TTY, closing stdin changes nothing.
        let mut bash = bash();
        assert_eq!(bash.close_stdin(), []);
        assert!(!bash.is_done());
    }

    #[test]
    fn a_line_left_when_stdin_closes_still_runs() {
        let (mut shell, _) = sh(false);
        shell.input(b"echo left");
        assert_eq!(
            shell.close_stdin(),
            vec![Event::Stdout(b"left\n".to_vec()), Event::Exit(0)]
        );
    }

    #[test]
    fn commands_split_at_separators_outside_quotes() {
        assert_eq!(
            split_commands("echo 'a;b' \"c d\"; exit 3\n\ntrue"),
            vec![
                argv(&["echo", "a;b", "c d"]),
                argv(&["exit", "3"]),
                argv(&["true"])
            ]
        );
        assert_eq!(split_commands("echo ''"), vec![argv(&["echo", ""])]);
        assert!(split_commands(" ; ").is_empty());
    }

    #[test]
    fn backspace_erases_whole_characters() {
        let mut line = "aé".as_bytes().to_vec();
        assert!(pop_char(&mut line));
        assert_eq!(line, b"a");
        assert!(pop_char(&mut line));
        assert!(!pop_char(&mut line));
    }
}
