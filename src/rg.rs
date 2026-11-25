//! A built-in `rg`, made from ripgrep's own crates (`grep`, `ignore`), for machines
//! without ripgrep installed. Shell commands crack runs find it on their PATH.
//!
//! It covers the flags agents and people use day to day, with ripgrep's output
//! format and exit codes (0 matched, 1 no match, 2 error). Anything else is
//! refused with a clear message rather than silently misread.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use grep::cli::StandardStream;
use grep::printer::{ColorSpecs, Standard, StandardBuilder, Summary, SummaryBuilder, SummaryKind};
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{BinaryDetection, Searcher, SearcherBuilder};
use termcolor::ColorChoice;

#[derive(Default)]
struct Options {
    patterns: Vec<String>,
    paths: Vec<PathBuf>,
    fixed: bool,
    ignore_case: bool,
    smart_case: bool,
    word: bool,
    line: bool,
    invert: bool,
    line_number: Option<bool>,
    column: bool,
    only_matching: bool,
    after: usize,
    before: usize,
    max_count: Option<u64>,
    max_columns: Option<u64>,
    max_depth: Option<usize>,
    multiline: bool,
    globs: Vec<String>,
    iglobs: Vec<String>,
    types: Vec<String>,
    types_not: Vec<String>,
    hidden: bool,
    no_ignore: bool,
    no_ignore_vcs: bool,
    follow: bool,
    text: bool,
    summary: Option<SummaryKind>,
    files: bool,
    type_list: bool,
    with_filename: Option<bool>,
    heading: Option<bool>,
    color: Option<String>,
    null: bool,
    trim: bool,
}

/// Runs `rg` with `args` (without the program name) and returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let options = match parse(args) {
        Ok(Some(options)) => options,
        Ok(None) => return 0,
        Err(message) => {
            eprintln!("rg: {message}");
            return 2;
        }
    };
    match run(options) {
        Ok(matched) => i32::from(!matched),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(error) => {
            eprintln!("rg: {error}");
            2
        }
    }
}

const USAGE: &str = "\
rg (built into crack; ripgrep-compatible)

usage: rg [OPTIONS] PATTERN [PATH ...]
       rg [OPTIONS] -e PATTERN ... [PATH ...]
       rg --files [PATH ...]

  -e, --regexp PAT     a pattern (repeatable)     -f, --file FILE   patterns from a file
  -F, --fixed-strings  literal patterns           -i / -s / -S      ignore / match / smart case
  -w, --word-regexp    whole words                -x, --line-regexp whole lines
  -v, --invert-match   non-matching lines         -U, --multiline   matches may span lines
  -n / -N              line numbers on / off      --column          show columns
  -o, --only-matching  only the matched parts     -m, --max-count N matches per file
  -A/-B/-C N           context lines              -M, --max-columns N
  -l / --files-without-match / -c / --count-matches / -q
  -g, --glob GLOB      include (or !exclude)      --iglob GLOB      case-insensitive glob
  -t TYPE / -T TYPE    file types                 --type-list
  -., --hidden         search hidden files        --no-ignore[-vcs] ignore .gitignore
  -u / -uu / -uuu      less filtering             -L, --follow      follow symlinks
  -a, --text           search binary files        -d, --max-depth N
  -H / -I              show / hide file names     --heading / --no-heading
  --color WHEN         never|auto|always          -0, --null        NUL after paths
  --trim               trim leading whitespace    --files           list files searched

Install ripgrep for its full feature set (rg is used instead when it's on PATH).";

fn parse(args: &[String]) -> Result<Option<Options>, String> {
    let mut o = Options::default();
    let mut positional = Vec::new();
    let mut pattern_given = false;
    let mut i = 0;
    let value = |i: &mut usize, inline: Option<String>, name: &str| -> Result<String, String> {
        if let Some(v) = inline {
            return Ok(v);
        }
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{name} needs a value"))
    };
    let number = |v: String, name: &str| -> Result<u64, String> {
        v.parse()
            .map_err(|_| format!("{name} needs a number, got '{v}'"))
    };
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            positional.extend(args[i + 1..].iter().cloned());
            break;
        }
        // Normalize short flags to (name, inline value) pairs, one per letter.
        let mut flags: Vec<(String, Option<String>)> = Vec::new();
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (long, None),
            };
            flags.push((format!("--{name}"), inline));
        } else if arg.len() > 1
            && arg.starts_with('-')
            && !arg[1..].starts_with(|c: char| c.is_ascii_digit())
        {
            let letters: Vec<char> = arg[1..].chars().collect();
            let mut k = 0;
            while k < letters.len() {
                let c = letters[k];
                let takes_value = "efgtTABCmMd".contains(c);
                if takes_value && k + 1 < letters.len() {
                    let rest: String = letters[k + 1..].iter().collect();
                    flags.push((format!("-{c}"), Some(rest)));
                    break;
                }
                flags.push((format!("-{c}"), None));
                k += 1;
            }
        } else {
            positional.push(arg.clone());
            i += 1;
            continue;
        }
        for (name, inline) in flags {
            match name.as_str() {
                "-h" | "--help" => {
                    println!("{USAGE}");
                    return Ok(None);
                }
                "-V" | "--version" => {
                    println!(
                        "ripgrep 15.1.0 (compatible subset; built into crack {})",
                        env!("CARGO_PKG_VERSION")
                    );
                    return Ok(None);
                }
                "-e" | "--regexp" => {
                    o.patterns.push(value(&mut i, inline, &name)?);
                    pattern_given = true;
                }
                "-f" | "--file" => {
                    let file = value(&mut i, inline, &name)?;
                    let text = if file == "-" {
                        io::read_to_string(io::stdin()).map_err(|e| e.to_string())?
                    } else {
                        std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?
                    };
                    o.patterns.extend(text.lines().map(str::to_owned));
                    pattern_given = true;
                }
                "-F" | "--fixed-strings" => o.fixed = true,
                "-i" | "--ignore-case" => (o.ignore_case, o.smart_case) = (true, false),
                "-s" | "--case-sensitive" => (o.ignore_case, o.smart_case) = (false, false),
                "-S" | "--smart-case" => (o.ignore_case, o.smart_case) = (false, true),
                "-w" | "--word-regexp" => o.word = true,
                "-x" | "--line-regexp" => o.line = true,
                "-v" | "--invert-match" => o.invert = true,
                "-n" | "--line-number" => o.line_number = Some(true),
                "-N" | "--no-line-number" => o.line_number = Some(false),
                "--column" => o.column = true,
                "-o" | "--only-matching" => o.only_matching = true,
                "-U" | "--multiline" => o.multiline = true,
                "-A" | "--after-context" => {
                    o.after = number(value(&mut i, inline, &name)?, &name)? as usize
                }
                "-B" | "--before-context" => {
                    o.before = number(value(&mut i, inline, &name)?, &name)? as usize
                }
                "-C" | "--context" => {
                    let n = number(value(&mut i, inline, &name)?, &name)? as usize;
                    (o.after, o.before) = (n, n);
                }
                "-m" | "--max-count" => {
                    o.max_count = Some(number(value(&mut i, inline, &name)?, &name)?)
                }
                "-M" | "--max-columns" => {
                    o.max_columns = Some(number(value(&mut i, inline, &name)?, &name)?)
                }
                "-d" | "--max-depth" | "--maxdepth" => {
                    o.max_depth = Some(number(value(&mut i, inline, &name)?, &name)? as usize)
                }
                "-g" | "--glob" => o.globs.push(value(&mut i, inline, &name)?),
                "--iglob" => o.iglobs.push(value(&mut i, inline, &name)?),
                "-t" | "--type" => o.types.push(value(&mut i, inline, &name)?),
                "-T" | "--type-not" => o.types_not.push(value(&mut i, inline, &name)?),
                "--type-list" => o.type_list = true,
                "-." | "--hidden" => o.hidden = true,
                "--no-ignore" => o.no_ignore = true,
                "--no-ignore-vcs" => o.no_ignore_vcs = true,
                "-u" | "--unrestricted" => {
                    if o.no_ignore && o.hidden {
                        o.text = true;
                    } else if o.no_ignore {
                        o.hidden = true;
                    } else {
                        o.no_ignore = true;
                    }
                }
                "-L" | "--follow" => o.follow = true,
                "-a" | "--text" | "--binary" => o.text = true,
                "-l" | "--files-with-matches" => o.summary = Some(SummaryKind::PathWithMatch),
                "--files-without-match" => o.summary = Some(SummaryKind::PathWithoutMatch),
                "-c" | "--count" => o.summary = Some(SummaryKind::Count),
                "--count-matches" => o.summary = Some(SummaryKind::CountMatches),
                "-q" | "--quiet" => o.summary = Some(SummaryKind::QuietWithMatch),
                "--files" => o.files = true,
                "-H" | "--with-filename" => o.with_filename = Some(true),
                "-I" | "--no-filename" => o.with_filename = Some(false),
                "--heading" => o.heading = Some(true),
                "--no-heading" => o.heading = Some(false),
                "--color" | "--colour" => o.color = Some(value(&mut i, inline, &name)?),
                "-0" | "--null" => o.null = true,
                "--trim" => o.trim = true,
                // Accepted for compatibility; this rg already behaves this way.
                "--no-messages" | "--sort-files" | "-j" | "--threads" | "--no-config" | "-p"
                | "--pretty" => {
                    if name == "-j" || name == "--threads" {
                        value(&mut i, inline, &name)?;
                    }
                    if name == "-p" || name == "--pretty" {
                        (o.heading, o.line_number, o.color) =
                            (Some(true), Some(true), Some("always".into()));
                    }
                }
                "--sort" => {
                    let how = value(&mut i, inline, &name)?;
                    if how != "path" && how != "none" {
                        return Err(format!(
                            "--sort {how} isn't supported here (only path); install ripgrep for it"
                        ));
                    }
                }
                other => {
                    return Err(format!(
                        "{other} isn't supported by crack's built-in rg; install ripgrep for the full set (rg --help lists what works)"
                    ));
                }
            }
        }
        i += 1;
    }
    if !pattern_given && !o.files && !o.type_list {
        if positional.is_empty() {
            return Err("no pattern given (rg --help for usage)".into());
        }
        o.patterns.push(positional.remove(0));
    }
    o.paths = positional.into_iter().map(PathBuf::from).collect();
    Ok(Some(o))
}

fn matcher(o: &Options) -> Result<RegexMatcher, String> {
    let pattern = if o.fixed {
        o.patterns
            .iter()
            .map(|p| regex::escape(p))
            .collect::<Vec<_>>()
            .join("|")
    } else if o.patterns.len() == 1 {
        o.patterns[0].clone()
    } else {
        o.patterns
            .iter()
            .map(|p| format!("(?:{p})"))
            .collect::<Vec<_>>()
            .join("|")
    };
    let mut builder = RegexMatcherBuilder::new();
    builder
        .case_insensitive(o.ignore_case)
        .case_smart(o.smart_case)
        .word(o.word)
        .whole_line(o.line)
        .multi_line(true);
    if o.multiline {
        builder.dot_matches_new_line(false);
    } else {
        builder.line_terminator(Some(b'\n'));
    }
    builder.build(&pattern).map_err(|e| e.to_string())
}

enum Printer {
    Standard(Standard<StandardStream>),
    Summary(Summary<StandardStream>),
}

fn run(o: Options) -> io::Result<bool> {
    let mut types = ignore::types::TypesBuilder::new();
    types.add_defaults();
    if o.type_list {
        let mut out = io::stdout().lock();
        for def in types.definitions() {
            writeln!(out, "{}: {}", def.name(), def.globs().join(", "))?;
        }
        return Ok(true);
    }
    for t in &o.types {
        types.select(t);
    }
    for t in &o.types_not {
        types.negate(t);
    }
    let types = types.build().map_err(io::Error::other)?;
    let cwd = std::env::current_dir()?;
    let mut overrides = ignore::overrides::OverrideBuilder::new(&cwd);
    for g in &o.globs {
        overrides.add(g).map_err(io::Error::other)?;
    }
    if !o.iglobs.is_empty() {
        overrides.case_insensitive(true).map_err(io::Error::other)?;
        for g in &o.iglobs {
            overrides.add(g).map_err(io::Error::other)?;
        }
    }
    let overrides = overrides.build().map_err(io::Error::other)?;

    let tty = io::stdout().is_terminal();
    let color = match o.color.as_deref() {
        Some("always") => ColorChoice::Always,
        Some("ansi") => ColorChoice::AlwaysAnsi,
        Some("never") => ColorChoice::Never,
        _ if tty => ColorChoice::Auto,
        _ => ColorChoice::Never,
    };
    let stdout = grep::cli::stdout(color);

    // No path: search stdin when it's piped in, else the current directory.
    let implicit = o.paths.is_empty();
    let search_stdin =
        implicit && !o.files && grep::cli::is_readable_stdin() && !io::stdin().is_terminal();
    let paths = if implicit {
        vec![PathBuf::from("./")]
    } else {
        o.paths.clone()
    };
    let with_filename = o
        .with_filename
        .unwrap_or(!search_stdin && (paths.len() > 1 || paths.iter().any(|p| p.is_dir())));

    if o.files {
        let mut out = stdout;
        let mut any = false;
        for path in &paths {
            for entry in walk(path, &o, &types, &overrides).flatten() {
                if entry.file_type().is_some_and(|t| t.is_file()) {
                    any = true;
                    write!(out, "{}", shown(entry.path(), implicit).display())?;
                    out.write_all(if o.null { b"\0" } else { b"\n" })?;
                }
            }
        }
        return Ok(any);
    }

    let matcher = matcher(&o).map_err(io::Error::other)?;
    let mut searcher = SearcherBuilder::new();
    searcher
        .line_number(o.line_number.unwrap_or(tty) || o.column)
        .invert_match(o.invert)
        .after_context(o.after)
        .before_context(o.before)
        .max_matches(o.max_count)
        .multi_line(o.multiline);
    let mut searcher = searcher.build();
    let mut printer = match o.summary {
        Some(kind) => Printer::Summary(
            SummaryBuilder::new()
                .kind(kind)
                .path(with_filename)
                .color_specs(ColorSpecs::default_with_color())
                .path_terminator(o.null.then_some(b'\0'))
                .build(stdout),
        ),
        None => Printer::Standard(
            StandardBuilder::new()
                .heading(o.heading.unwrap_or(tty) && with_filename)
                .path(with_filename)
                .column(o.column)
                .only_matching(o.only_matching)
                .max_columns(o.max_columns)
                .trim_ascii(o.trim)
                .color_specs(ColorSpecs::default_with_color())
                .path_terminator(o.null.then_some(b'\0'))
                .build(stdout),
        ),
    };

    let mut matched = false;
    let mut failed = false;
    if search_stdin {
        searcher.set_binary_detection(binary(&o, true));
        matched |= search(&mut searcher, &matcher, &mut printer, None, &mut failed)?;
    } else {
        for root in &paths {
            let explicit_file = root.is_file();
            for entry in walk(root, &o, &types, &overrides) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        eprintln!("rg: {error}");
                        failed = true;
                        continue;
                    }
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                searcher.set_binary_detection(binary(&o, explicit_file));
                let path = shown(entry.path(), implicit);
                matched |= search(
                    &mut searcher,
                    &matcher,
                    &mut printer,
                    Some(&path),
                    &mut failed,
                )?;
                if matched && matches!(o.summary, Some(SummaryKind::QuietWithMatch)) {
                    return Ok(true);
                }
            }
        }
    }
    if failed && !matched {
        return Err(io::Error::other("some files couldn't be searched"));
    }
    Ok(matched)
}

fn binary(o: &Options, explicit: bool) -> BinaryDetection {
    if o.text {
        BinaryDetection::none()
    } else if explicit {
        BinaryDetection::convert(b'\0')
    } else {
        BinaryDetection::quit(b'\0')
    }
}

fn shown(path: &Path, implicit: bool) -> PathBuf {
    if implicit {
        path.strip_prefix("./").unwrap_or(path).to_owned()
    } else {
        path.to_owned()
    }
}

fn walk(
    root: &Path,
    o: &Options,
    types: &ignore::types::Types,
    overrides: &ignore::overrides::Override,
) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!o.hidden)
        .ignore(!o.no_ignore)
        .git_ignore(!o.no_ignore && !o.no_ignore_vcs)
        .git_exclude(!o.no_ignore && !o.no_ignore_vcs)
        .git_global(!o.no_ignore && !o.no_ignore_vcs)
        .parents(!o.no_ignore)
        .follow_links(o.follow)
        .max_depth(o.max_depth)
        .types(types.clone())
        .overrides(overrides.clone())
        .sort_by_file_name(|a, b| a.cmp(b));
    builder.build()
}

fn search(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    printer: &mut Printer,
    path: Option<&Path>,
    failed: &mut bool,
) -> io::Result<bool> {
    let result = match (printer, path) {
        (Printer::Standard(p), Some(path)) => {
            let mut sink = p.sink_with_path(matcher, path);
            searcher
                .search_path(matcher, path, &mut sink)
                .map(|_| sink.has_match())
        }
        (Printer::Standard(p), None) => {
            let mut sink = p.sink(matcher);
            searcher
                .search_reader(matcher, io::stdin(), &mut sink)
                .map(|_| sink.has_match())
        }
        (Printer::Summary(p), Some(path)) => {
            let mut sink = p.sink_with_path(matcher, path);
            searcher
                .search_path(matcher, path, &mut sink)
                .map(|_| sink.has_match())
        }
        (Printer::Summary(p), None) => {
            let mut sink = p.sink_with_path(matcher, Path::new("<stdin>"));
            searcher
                .search_reader(matcher, io::stdin(), &mut sink)
                .map(|_| sink.has_match())
        }
    };
    match result {
        Ok(found) => Ok(found),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Err(error),
        Err(error) => {
            if let Some(path) = path {
                eprintln!("rg: {}: {error}", path.display());
            } else {
                eprintln!("rg: {error}");
            }
            *failed = true;
            Ok(false)
        }
    }
}

/// A folder holding an `rg` link to crack, for PATH, when ripgrep isn't installed.
pub fn shim_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if crate::policy::which("rg").is_some() {
            return None;
        }
        let dir = crate::config::data_dir().join("bin");
        let link = dir.join("rg");
        let exe = std::env::current_exe().ok()?;
        std::fs::create_dir_all(&dir).ok()?;
        if std::fs::read_link(&link).ok().as_deref() != Some(exe.as_path()) {
            let _ = std::fs::remove_file(&link);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&exe, &link).ok()?;
        }
        Some(dir)
    })
    .clone()
}
