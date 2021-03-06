use std::io::BufRead;
use std::io::Write;

use bytelines::ByteLines;
use console::strip_ansi_codes;
use unicode_segmentation::UnicodeSegmentation;

use crate::config::Config;
use crate::draw;
use crate::paint::Painter;
use crate::parse;
use crate::style::DecorationStyle;

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    CommitMeta, // In commit metadata section
    FileMeta,   // In diff metadata section, between (possible) commit metadata and first hunk
    HunkHeader, // In hunk metadata line
    HunkZero,   // In hunk; unchanged line
    HunkMinus,  // In hunk; removed line
    HunkPlus,   // In hunk; added line
    Unknown,
}

#[derive(Debug, PartialEq)]
pub enum Source {
    GitDiff,     // Coming from a `git diff` command
    DiffUnified, // Coming from a `diff -u` command
    Unknown,
}

impl State {
    fn is_in_hunk(&self) -> bool {
        match *self {
            State::HunkHeader | State::HunkZero | State::HunkMinus | State::HunkPlus => true,
            _ => false,
        }
    }
}

// Possible transitions, with actions on entry:
//
//
// | from \ to   | CommitMeta  | FileMeta    | HunkHeader  | HunkZero    | HunkMinus   | HunkPlus |
// |-------------+-------------+-------------+-------------+-------------+-------------+----------|
// | CommitMeta  | emit        | emit        |             |             |             |          |
// | FileMeta    |             | emit        | emit        |             |             |          |
// | HunkHeader  |             |             |             | emit        | push        | push     |
// | HunkZero    | emit        | emit        | emit        | emit        | push        | push     |
// | HunkMinus   | flush, emit | flush, emit | flush, emit | flush, emit | push        | push     |
// | HunkPlus    | flush, emit | flush, emit | flush, emit | flush, emit | flush, push | push     |

pub fn delta<I>(
    mut lines: ByteLines<I>,
    writer: &mut dyn Write,
    config: &Config,
) -> std::io::Result<()>
where
    I: BufRead,
{
    let mut painter = Painter::new(writer, config);
    let mut minus_file = "".to_string();
    let mut plus_file;
    let mut state = State::Unknown;
    let mut source = Source::Unknown;

    while let Some(Ok(raw_line_bytes)) = lines.next() {
        let raw_line = String::from_utf8_lossy(&raw_line_bytes);
        let line = strip_ansi_codes(&raw_line).to_string();
        if source == Source::Unknown {
            source = detect_source(&line);
        }
        if line.starts_with("commit ") {
            painter.paint_buffered_lines();
            state = State::CommitMeta;
            if should_handle(&state, config) {
                painter.emit()?;
                handle_commit_meta_header_line(&mut painter, &line, &raw_line, config)?;
                continue;
            }
        } else if line.starts_with("diff ") {
            painter.paint_buffered_lines();
            state = State::FileMeta;
        } else if (state == State::FileMeta || source == Source::DiffUnified)
            && (line.starts_with("--- ") || line.starts_with("rename from "))
            && should_handle(&State::FileMeta, config)
        {
            minus_file = parse::get_file_path_from_file_meta_line(&line, source == Source::GitDiff);
            if source == Source::DiffUnified {
                state = State::FileMeta;
                painter.set_syntax(parse::get_file_extension_from_marker_line(&line));
            } else {
                state = State::FileMeta;
                painter.set_syntax(parse::get_file_extension_from_file_meta_line_file_path(
                    &minus_file,
                ));
            }
        } else if (line.starts_with("+++ ") || line.starts_with("rename to "))
            && should_handle(&State::FileMeta, config)
        {
            plus_file = parse::get_file_path_from_file_meta_line(&line, source == Source::GitDiff);
            painter.set_syntax(parse::get_file_extension_from_file_meta_line_file_path(
                &plus_file,
            ));
            painter.emit()?;
            handle_file_meta_header_line(
                &mut painter,
                &minus_file,
                &plus_file,
                config,
                source == Source::DiffUnified,
            )?;
        } else if line.starts_with("@@") {
            state = State::HunkHeader;
            painter.set_highlighter();
            if should_handle(&state, config) {
                painter.emit()?;
                handle_hunk_header_line(&mut painter, &line, &raw_line, config)?;
                continue;
            }
        } else if source == Source::DiffUnified && line.starts_with("Only in ")
            || line.starts_with("Submodule ")
            || line.starts_with("Binary files ")
        {
            // Additional FileMeta cases:
            //
            // 1. When comparing directories with diff -u, if filenames match between the
            //    directories, the files themselves will be compared. However, if an equivalent
            //    filename is not present, diff outputs a single line (Only in...) starting
            //    indicating that the file is present in only one of the directories.
            //
            // 2. Git diff emits lines describing submodule state such as "Submodule x/y/z contains
            //    untracked content"
            //
            // See https://github.com/dandavison/delta/issues/60#issuecomment-557485242 for a
            // proposal for more robust parsing logic.

            state = State::FileMeta;
            painter.paint_buffered_lines();
            if should_handle(&State::FileMeta, config) {
                painter.emit()?;
                handle_generic_file_meta_header_line(&mut painter, &line, &raw_line, config)?;
                continue;
            }
        } else if state.is_in_hunk() {
            // A true hunk line should start with one of: '+', '-', ' '. However, handle_hunk_line
            // handles all lines until the state machine transitions away from the hunk states.
            state = handle_hunk_line(&mut painter, &line, &raw_line, state, config);
            painter.emit()?;
            continue;
        }

        if state == State::FileMeta && should_handle(&State::FileMeta, config) {
            // The file metadata section is 4 lines. Skip them under non-plain file-styles.
            continue;
        } else {
            painter.emit()?;
            writeln!(painter.writer, "{}", raw_line)?;
        }
    }

    painter.paint_buffered_lines();
    painter.emit()?;
    Ok(())
}

/// Should a handle_* function be called on this element?
fn should_handle(state: &State, config: &Config) -> bool {
    let style = config.get_style(state);
    !(style.is_raw && style.decoration_style == DecorationStyle::NoDecoration)
}

/// Try to detect what is producing the input for delta.
///
/// Currently can detect:
/// * git diff
/// * diff -u
fn detect_source(line: &str) -> Source {
    if line.starts_with("commit ") || line.starts_with("diff --git ") {
        Source::GitDiff
    } else if line.starts_with("diff -u ")
        || line.starts_with("diff -U")
        || line.starts_with("--- ")
    {
        Source::DiffUnified
    } else {
        Source::Unknown
    }
}

fn handle_commit_meta_header_line(
    painter: &mut Painter,
    line: &str,
    raw_line: &str,
    config: &Config,
) -> std::io::Result<()> {
    if config.commit_style.is_omitted {
        return Ok(());
    }
    let decoration_ansi_term_style;
    let mut pad = false;
    let draw_fn = match config.commit_style.decoration_style {
        DecorationStyle::Box(style) => {
            pad = true;
            decoration_ansi_term_style = style;
            draw::write_boxed
        }
        DecorationStyle::BoxWithUnderline(style) => {
            pad = true;
            decoration_ansi_term_style = style;
            draw::write_boxed_with_underline
        }
        DecorationStyle::BoxWithOverline(style) => {
            pad = true;
            decoration_ansi_term_style = style;
            draw::write_boxed // TODO: not implemented
        }
        DecorationStyle::BoxWithUnderOverline(style) => {
            pad = true;
            decoration_ansi_term_style = style;
            draw::write_boxed // TODO: not implemented
        }
        DecorationStyle::Underline(style) => {
            decoration_ansi_term_style = style;
            draw::write_underlined
        }
        DecorationStyle::Overline(style) => {
            decoration_ansi_term_style = style;
            draw::write_overlined
        }
        DecorationStyle::UnderOverline(style) => {
            decoration_ansi_term_style = style;
            draw::write_underoverlined
        }
        DecorationStyle::NoDecoration => {
            decoration_ansi_term_style = ansi_term::Style::new();
            draw::write_no_decoration
        }
    };
    draw_fn(
        painter.writer,
        &format!("{}{}", line, if pad { " " } else { "" }),
        &format!("{}{}", raw_line, if pad { " " } else { "" }),
        &config.decorations_width,
        config.commit_style,
        decoration_ansi_term_style,
    )?;
    Ok(())
}

/// Construct file change line from minus and plus file and write with FileMeta styling.
fn handle_file_meta_header_line(
    painter: &mut Painter,
    minus_file: &str,
    plus_file: &str,
    config: &Config,
    comparing: bool,
) -> std::io::Result<()> {
    let line = parse::get_file_change_description_from_file_paths(minus_file, plus_file, comparing);
    // FIXME: no support for 'raw'
    handle_generic_file_meta_header_line(painter, &line, &line, config)
}

/// Write `line` with FileMeta styling.
fn handle_generic_file_meta_header_line(
    painter: &mut Painter,
    line: &str,
    raw_line: &str,
    config: &Config,
) -> std::io::Result<()> {
    if config.file_style.is_omitted {
        return Ok(());
    }
    let decoration_ansi_term_style;
    let mut pad = false;
    let draw_fn = match config.file_style.decoration_style {
        DecorationStyle::Box(style) => {
            pad = true;
            decoration_ansi_term_style = style;
            draw::write_boxed
        }
        DecorationStyle::BoxWithUnderline(style) => {
            pad = true;
            decoration_ansi_term_style = style;
            draw::write_boxed_with_underline
        }
        DecorationStyle::BoxWithOverline(style) => {
            pad = true;
            decoration_ansi_term_style = style;
            draw::write_boxed // TODO: not implemented
        }
        DecorationStyle::BoxWithUnderOverline(style) => {
            pad = true;
            decoration_ansi_term_style = style;
            draw::write_boxed // TODO: not implemented
        }
        DecorationStyle::Underline(style) => {
            decoration_ansi_term_style = style;
            draw::write_underlined
        }
        DecorationStyle::Overline(style) => {
            decoration_ansi_term_style = style;
            draw::write_overlined
        }
        DecorationStyle::UnderOverline(style) => {
            decoration_ansi_term_style = style;
            draw::write_underoverlined
        }
        DecorationStyle::NoDecoration => {
            decoration_ansi_term_style = ansi_term::Style::new();
            draw::write_no_decoration
        }
    };
    writeln!(painter.writer)?;
    draw_fn(
        painter.writer,
        &format!("{}{}", line, if pad { " " } else { "" }),
        &format!("{}{}", raw_line, if pad { " " } else { "" }),
        &config.decorations_width,
        config.file_style,
        decoration_ansi_term_style,
    )?;
    Ok(())
}

fn handle_hunk_header_line(
    painter: &mut Painter,
    line: &str,
    raw_line: &str,
    config: &Config,
) -> std::io::Result<()> {
    if config.hunk_header_style.is_omitted {
        return Ok(());
    }
    let decoration_ansi_term_style;
    let draw_fn = match config.hunk_header_style.decoration_style {
        DecorationStyle::Box(style) => {
            decoration_ansi_term_style = style;
            draw::write_boxed
        }
        DecorationStyle::BoxWithUnderline(style) => {
            decoration_ansi_term_style = style;
            draw::write_boxed_with_underline
        }
        DecorationStyle::BoxWithOverline(style) => {
            decoration_ansi_term_style = style;
            draw::write_boxed // TODO: not implemented
        }
        DecorationStyle::BoxWithUnderOverline(style) => {
            decoration_ansi_term_style = style;
            draw::write_boxed // TODO: not implemented
        }
        DecorationStyle::Underline(style) => {
            decoration_ansi_term_style = style;
            draw::write_underlined
        }
        DecorationStyle::Overline(style) => {
            decoration_ansi_term_style = style;
            draw::write_overlined
        }
        DecorationStyle::UnderOverline(style) => {
            decoration_ansi_term_style = style;
            draw::write_underoverlined
        }
        DecorationStyle::NoDecoration => {
            decoration_ansi_term_style = ansi_term::Style::new();
            draw::write_no_decoration
        }
    };
    let (raw_code_fragment, line_number) = parse::parse_hunk_metadata(&line);
    if config.hunk_header_style.is_raw {
        writeln!(painter.writer)?;
        draw_fn(
            painter.writer,
            &format!("{} ", line),
            &format!("{} ", raw_line),
            &config.decorations_width,
            config.hunk_header_style,
            decoration_ansi_term_style,
        )?;
    } else {
        let line = match prepare(raw_code_fragment, false, config) {
            s if s.len() > 0 => format!("{} ", s),
            s => s,
        };
        writeln!(painter.writer)?;
        if !line.is_empty() {
            let lines = vec![line];
            let syntax_style_sections = Painter::get_syntax_style_sections_for_lines(
                &lines,
                &State::HunkHeader,
                &mut painter.highlighter,
                &painter.config,
            );
            Painter::paint_lines(
                syntax_style_sections,
                vec![vec![(config.hunk_header_style, &lines[0])]],
                &mut painter.output_buffer,
                config,
                "",
                config.null_style,
                config.null_style,
                Some(false),
            );
            painter.output_buffer.pop(); // trim newline
            draw_fn(
                painter.writer,
                &painter.output_buffer,
                &painter.output_buffer,
                &config.decorations_width,
                config.hunk_header_style,
                decoration_ansi_term_style,
            )?;
            if !config.hunk_header_style.is_raw {
                painter.output_buffer.clear()
            };
        }
    };
    match config.hunk_header_style.decoration_ansi_term_style() {
        Some(style) => writeln!(painter.writer, "{}", style.paint(line_number))?,
        None => writeln!(painter.writer, "{}", line_number)?,
    };
    Ok(())
}

/// Handle a hunk line, i.e. a minus line, a plus line, or an unchanged line.
// In the case of a minus or plus line, we store the line in a
// buffer. When we exit the changed region we process the collected
// minus and plus lines jointly, in order to paint detailed
// highlighting according to inferred edit operations. In the case of
// an unchanged line, we paint it immediately.
fn handle_hunk_line(
    painter: &mut Painter,
    line: &str,
    raw_line: &str,
    state: State,
    config: &Config,
) -> State {
    // Don't let the line buffers become arbitrarily large -- if we
    // were to allow that, then for a large deleted/added file we
    // would process the entire file before painting anything.
    if painter.minus_lines.len() > config.max_buffered_lines
        || painter.plus_lines.len() > config.max_buffered_lines
    {
        painter.paint_buffered_lines();
    }
    match line.chars().next() {
        Some('-') => {
            if state == State::HunkPlus {
                painter.paint_buffered_lines();
            }
            painter.minus_lines.push(prepare(&line, true, config));
            State::HunkMinus
        }
        Some('+') => {
            painter.plus_lines.push(prepare(&line, true, config));
            State::HunkPlus
        }
        Some(' ') => {
            let state = State::HunkZero;
            let prefix = if line.is_empty() { "" } else { &line[..1] };
            painter.paint_buffered_lines();
            let lines = vec![prepare(&line, true, config)];
            let syntax_style_sections = Painter::get_syntax_style_sections_for_lines(
                &lines,
                &state,
                &mut painter.highlighter,
                &painter.config,
            );
            let diff_style_sections = vec![(config.zero_style, lines[0].as_str())];

            Painter::paint_lines(
                syntax_style_sections,
                vec![diff_style_sections],
                &mut painter.output_buffer,
                config,
                prefix,
                config.zero_style,
                config.zero_style,
                None,
            );
            state
        }
        _ => {
            // The first character here could be e.g. '\' from '\ No newline at end of file'. This
            // is not a hunk line, but the parser does not have a more accurate state corresponding
            // to this.
            painter.paint_buffered_lines();
            painter
                .output_buffer
                .push_str(&expand_tabs(raw_line.graphemes(true), config.tab_width));
            painter.output_buffer.push_str("\n");
            State::HunkZero
        }
    }
}

/// Replace initial -/+ character with ' ', expand tabs as spaces, and optionally terminate with
/// newline.
// Terminating with newline character is necessary for many of the sublime syntax definitions to
// highlight correctly.
// See https://docs.rs/syntect/3.2.0/syntect/parsing/struct.SyntaxSetBuilder.html#method.add_from_folder
fn prepare(line: &str, append_newline: bool, config: &Config) -> String {
    let terminator = if append_newline { "\n" } else { "" };
    if !line.is_empty() {
        let mut line = line.graphemes(true);

        // The first column contains a -/+/space character, added by git. We substitute it for a
        // space now, so that it is not present during syntax highlighting, and substitute again
        // when emitting the line.
        line.next();

        format!(" {}{}", expand_tabs(line, config.tab_width), terminator)
    } else {
        terminator.to_string()
    }
}

/// Expand tabs as spaces.
/// tab_width = 0 is documented to mean do not replace tabs.
fn expand_tabs<'a, I>(line: I, tab_width: usize) -> String
where
    I: Iterator<Item = &'a str>,
{
    if tab_width > 0 {
        let tab_replacement = " ".repeat(tab_width);
        line.map(|s| if s == "\t" { &tab_replacement } else { s })
            .collect::<String>()
    } else {
        line.collect::<String>()
    }
}
