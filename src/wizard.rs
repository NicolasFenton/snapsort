//! Interactive setup for running plain `snapsort`: asks, one question at a
//! time, for whatever the command line didn't provide.

use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

use crate::{
    categories::{Categories, UNUSABLE},
    scan,
    ui::Paint,
};

/// Used when the user just presses Enter at the categories question.
pub const DEFAULT_CATEGORIES: &str = include_str!("../examples/categories.json");

/// What the command line already decided.
#[derive(Debug, Default)]
pub struct Given {
    pub input: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub categories: Option<PathBuf>,
    pub copy: bool,
    pub dry_run: bool,
    pub recursive: bool,
    pub unusable: bool,
}

#[derive(Debug)]
pub struct Setup {
    pub input: PathBuf,
    pub output: PathBuf,
    pub categories: Categories,
    pub copy: bool,
    /// Put unusable photos in their own folder.
    pub unusable: bool,
}

#[derive(Debug)]
pub enum Answer {
    Start(Setup),
    Cancelled,
}

/// Ask on the terminal (questions on stderr, answers from stdin).
pub fn run(given: &Given) -> Result<Answer> {
    let stdin = io::stdin();
    let cwd = std::env::current_dir().context("cannot determine the current folder")?;
    ask(given, &mut stdin.lock(), &mut io::stderr(), &cwd)
}

pub fn ask(
    given: &Given,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    cwd: &Path,
) -> Result<Answer> {
    let mut prompt = Prompt {
        input,
        out,
        paint: Paint::stderr(),
    };

    let folder = match &given.input {
        Some(path) => path.clone(),
        None => prompt.folder(cwd, given.recursive)?,
    };
    let categories = match &given.categories {
        Some(path) => Categories::load(path)?,
        None => prompt.categories(cwd)?,
    };
    let output = match &given.output {
        Some(path) => path.clone(),
        None => prompt.destination(cwd, &folder)?,
    };
    let copy = if given.copy || given.dry_run {
        given.copy
    } else {
        prompt.copy()?
    };
    let unusable = given.unusable || prompt.unusable()?;

    let action = match (given.dry_run, copy) {
        (true, _) => "Preview (nothing will be moved):",
        (false, true) => "Copy",
        (false, false) => "Move",
    };
    writeln!(
        prompt.out,
        "\n{action} images from {} into {} folders in {}{}",
        pretty(&folder),
        categories.len(),
        pretty(&output),
        if unusable {
            format!(", and blurry or bad photos into {UNUSABLE}")
        } else {
            String::new()
        }
    )?;
    if !prompt.confirm("Start?")? {
        return Ok(Answer::Cancelled);
    }
    writeln!(prompt.out)?;
    Ok(Answer::Start(Setup {
        input: folder,
        output,
        categories,
        copy,
        unusable,
    }))
}

struct Prompt<'a> {
    input: &'a mut dyn BufRead,
    out: &'a mut dyn Write,
    paint: Paint,
}

impl Prompt<'_> {
    /// One line of input; Enter alone picks `default`. End of input cancels.
    fn line(&mut self, question: &str, default: Option<&str>) -> Result<String> {
        match default {
            Some(default) => write!(
                self.out,
                "{} {}: ",
                self.paint.bold(question),
                self.paint.dim(&format!("[{default}]"))
            )?,
            None => write!(self.out, "{}: ", self.paint.bold(question))?,
        }
        self.out.flush()?;
        let mut answer = String::new();
        if self.input.read_line(&mut answer)? == 0 {
            bail!("setup cancelled");
        }
        let answer = answer.trim();
        Ok(if answer.is_empty() {
            default.unwrap_or_default().to_owned()
        } else {
            answer.to_owned()
        })
    }

    fn retry(&mut self, message: impl AsRef<str>) -> Result<()> {
        writeln!(self.out, "  {}", self.paint.yellow(message.as_ref()))?;
        Ok(())
    }

    fn folder(&mut self, cwd: &Path, recursive: bool) -> Result<PathBuf> {
        let current = pretty(cwd);
        loop {
            let path = expand_path(&self.line("Folder to sort", Some(&current))?, cwd);
            if !path.is_dir() {
                self.retry(format!("{} is not a folder. Try again.", path.display()))?;
                continue;
            }
            let found = scan::scan(&path, recursive, &[]).files.len();
            if found == 0 {
                let hint = if recursive {
                    ""
                } else {
                    " (subfolders aren't searched unless you pass --recursive)"
                };
                self.retry(format!(
                    "No JPEG, PNG, WebP or camera raw images there{hint}. Try another folder."
                ))?;
                continue;
            }
            writeln!(
                self.out,
                "  Found {found} image{}.",
                if found == 1 { "" } else { "s" }
            )?;
            return Ok(path);
        }
    }

    fn categories(&mut self, cwd: &Path) -> Result<Categories> {
        let defaults =
            Categories::parse(DEFAULT_CATEGORIES).expect("built-in categories are valid");
        let names: Vec<&str> = defaults.iter().map(|c| c.name.as_str()).collect();
        writeln!(
            self.out,
            "\n{}",
            self.paint
                .bold("Which folders should images be sorted into?")
        )?;
        writeln!(
            self.out,
            "  {}  {}",
            self.paint.dim("Enter      "),
            names.join(", ")
        )?;
        writeln!(
            self.out,
            "  {}  your own folder names, separated by commas",
            self.paint.dim("a, b, c    ")
        )?;
        writeln!(
            self.out,
            "  {}  a categories file with descriptions",
            self.paint.dim("file.json  ")
        )?;
        loop {
            let answer = self.line("Folders", Some("defaults"))?;
            let parsed = if answer == "defaults" {
                Ok(defaults.clone())
            } else if looks_like_file(&answer, cwd) {
                Categories::load(&expand_path(&answer, cwd))
            } else {
                categories_from_names(&answer)
            };
            match parsed {
                Ok(categories) => return Ok(categories),
                Err(err) => self.retry(format!("{err:#}. Try again."))?,
            }
        }
    }

    fn destination(&mut self, cwd: &Path, folder: &Path) -> Result<PathBuf> {
        let same = pretty(folder);
        loop {
            let path = expand_path(&self.line("\nPut the sorted folders in", Some(&same))?, cwd);
            if path.exists() && !path.is_dir() {
                self.retry(format!(
                    "{} is a file, not a folder. Try again.",
                    path.display()
                ))?;
                continue;
            }
            return Ok(path);
        }
    }

    fn copy(&mut self) -> Result<bool> {
        loop {
            match self
                .line(
                    "Move the images, or copy them and keep the originals? (move/copy)",
                    Some("move"),
                )?
                .to_ascii_lowercase()
                .as_str()
            {
                "m" | "move" => return Ok(false),
                "c" | "copy" => return Ok(true),
                _ => self.retry("Type move or copy.")?,
            }
        }
    }

    fn unusable(&mut self) -> Result<bool> {
        self.yes_no(
            &format!("Put blurry, badly exposed or accidental photos in an {UNUSABLE} folder?"),
            false,
        )
    }

    fn confirm(&mut self, question: &str) -> Result<bool> {
        self.yes_no(question, true)
    }

    fn yes_no(&mut self, question: &str, default: bool) -> Result<bool> {
        let hint = if default { "Y/n" } else { "y/N" };
        loop {
            let answer = self.line(question, Some(hint))?;
            if answer == hint {
                return Ok(default);
            }
            match answer.to_ascii_lowercase().as_str() {
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.retry("Type y or n.")?,
            }
        }
    }
}

fn categories_from_names(answer: &str) -> Result<Categories> {
    let entries: Map<String, Value> = answer
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| (name.to_owned(), Value::Null))
        .collect();
    Categories::parse(&Value::Object(entries).to_string())
}

fn looks_like_file(answer: &str, cwd: &Path) -> bool {
    answer
        .to_ascii_lowercase()
        .trim_end_matches(['"', '\''])
        .ends_with(".json")
        || expand_path(answer, cwd).is_file()
}

/// Show paths under the home folder as `~/...`, which is shorter to read
/// and is accepted back as an answer.
fn pretty(path: &Path) -> String {
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// Accept paths the way people type or paste them: `~/x`, relative to the
/// current folder, wrapped in quotes, or with `\ `-escaped spaces (what macOS
/// Terminal inserts when you drag a folder in).
pub fn expand_path(raw: &str, cwd: &Path) -> PathBuf {
    let mut text = raw.trim();
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            text = &text[1..text.len() - 1];
        }
    }
    let unescaped: String = if cfg!(windows) {
        text.to_owned()
    } else {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(c);
            }
        }
        out
    };

    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    let path = match (unescaped.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) if rest.starts_with('/') || rest.starts_with('\\') => {
            home.join(&rest[1..])
        }
        _ => PathBuf::from(&unescaped),
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Cursor};

    fn photos() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.jpg", "b.png"] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        dir
    }

    fn run_script(given: &Given, script: &str, cwd: &Path) -> (Result<Answer>, String) {
        let mut input = Cursor::new(script.as_bytes().to_vec());
        let mut out = Vec::new();
        let answer = ask(given, &mut input, &mut out, cwd);
        (answer, String::from_utf8(out).unwrap())
    }

    #[test]
    fn pressing_enter_everywhere_sorts_the_current_folder_in_place() {
        let dir = photos();
        let (answer, transcript) = run_script(&Given::default(), "\n\n\n\n\n\n", dir.path());
        let Answer::Start(setup) = answer.unwrap() else {
            panic!("cancelled")
        };
        assert_eq!(setup.input, dir.path());
        assert_eq!(setup.output, dir.path());
        assert_eq!(
            setup.categories.len(),
            Categories::parse(DEFAULT_CATEGORIES).unwrap().len()
        );
        assert!(!setup.copy);
        assert!(!setup.unusable, "the unusable folder is opt-in");
        assert!(transcript.contains("Found 2 images"), "{transcript}");
    }

    #[test]
    fn custom_names_destination_copy_and_unusable() {
        let dir = photos();
        let script = format!(
            "{}\ncats, dogs ,\nsorted\ncopy\nyes\ny\n",
            dir.path().display()
        );
        let (answer, transcript) = run_script(&Given::default(), &script, Path::new("/"));
        let Answer::Start(setup) = answer.unwrap() else {
            panic!("cancelled")
        };
        let names: Vec<_> = setup.categories.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, ["cats", "dogs"]);
        assert_eq!(setup.output, Path::new("/sorted"));
        assert!(setup.copy);
        assert!(setup.unusable);
        assert!(
            transcript.contains("and blurry or bad photos into unusable"),
            "{transcript}"
        );
    }

    #[test]
    fn re_asks_after_bad_answers_and_can_cancel() {
        let dir = photos();
        let empty = tempfile::tempdir().unwrap();
        let script = format!(
            "/definitely/not/here\n{}\n{}\nonlyone\ncats,dogs\n\nmaybe\nm\nsure\nn\nn\n",
            empty.path().display(),
            dir.path().display()
        );
        let (answer, transcript) = run_script(&Given::default(), &script, Path::new("/"));
        assert!(matches!(answer.unwrap(), Answer::Cancelled));
        assert!(transcript.contains("is not a folder"), "{transcript}");
        assert!(transcript.contains("No JPEG, PNG"), "{transcript}");
        assert!(transcript.contains("between 2 and 255"), "{transcript}");
        assert!(transcript.contains("Type move or copy"), "{transcript}");
        assert!(transcript.contains("Type y or n"), "{transcript}");
    }

    #[test]
    fn skips_questions_answered_by_flags() {
        let dir = photos();
        let given = Given {
            input: Some(dir.path().to_path_buf()),
            output: Some(dir.path().join("out")),
            dry_run: true,
            unusable: true,
            ..Given::default()
        };
        // Only the categories question and the confirmation remain.
        let (answer, transcript) = run_script(&given, "a,b\n\n", Path::new("/"));
        let Answer::Start(setup) = answer.unwrap() else {
            panic!("cancelled")
        };
        assert!(setup.unusable);
        assert!(!transcript.contains("Folder to sort") && !transcript.contains("Move the images"));
        assert!(!transcript.contains("badly exposed"), "{transcript}");
        assert!(transcript.contains("Preview"));
    }

    #[test]
    fn end_of_input_cancels() {
        let (answer, _) = run_script(&Given::default(), "", Path::new("/"));
        assert!(answer.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn expands_pasted_and_dragged_paths() {
        let cwd = Path::new("/work");
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(expand_path("photos", cwd), Path::new("/work/photos"));
        assert_eq!(expand_path("~/Pictures", cwd), home.join("Pictures"));
        assert_eq!(expand_path("~", cwd), home);
        assert_eq!(expand_path("'/a b/c'", cwd), Path::new("/a b/c"));
        assert_eq!(
            expand_path("/Users/me/My\\ Photos\\ \\(2024\\) ", cwd),
            Path::new("/Users/me/My Photos (2024)")
        );
        assert_eq!(pretty(&home.join("Pictures")), "~/Pictures");
        assert_eq!(
            expand_path(&pretty(&home.join("Pictures")), cwd),
            home.join("Pictures")
        );
        assert_eq!(pretty(Path::new("/tmp/x")), "/tmp/x");
    }
}
