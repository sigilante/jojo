use nockapp::kernel::boot;
use nockapp::NockApp;
use nockapp::noun::slab::NounSlab;
use nockapp::wire::{SystemWire, Wire};
use nockapp::{AtomExt};
use nockvm::noun::{Atom, D, T};
use nockvm_macros::tas;
use std::error::Error;
use std::fs;
use bytes::Bytes;
use rustyline::error::ReadlineError;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Completer, Editor, Helper, Highlighter, Hinter};

fn string_to_atom(slab: &mut NounSlab, s: &str) -> Result<Atom, Box<dyn Error>> {
  let bytes = Bytes::from(s.as_bytes().to_vec());
  Ok(Atom::from_bytes(slab, &bytes))
}

// Net bracket depth of a chunk, for multiline continuation:
// counts { ( [ against } ) ] outside string ("...") and char
// ('...') literals, ignoring // line comments.  Positive depth
// means the entry is structurally open and jojo keeps reading;
// zero or negative submits (a genuinely unbalanced entry should
// fail in the kernel, visibly, not trap the user at a
// continuation prompt — same for an unterminated quote, whose
// suppressed brackets leave depth wherever it stood).
fn open_depth(src: &str) -> i32 {
  enum St { Normal, Dq, Sq }
  let mut depth = 0i32;
  let mut st = St::Normal;
  let mut chars = src.chars().peekable();
  while let Some(c) = chars.next() {
    match st {
      St::Normal => match c {
        '/' if chars.peek() == Some(&'/') => {
          while let Some(&n) = chars.peek() {
            if n == '\n' { break; }
            chars.next();
          }
        }
        '"' => st = St::Dq,
        '\'' => st = St::Sq,
        '{' | '(' | '[' => depth += 1,
        '}' | ')' | ']' => depth -= 1,
        _ => {}
      },
      St::Dq => match c {
        '\\' => { chars.next(); }
        '"' => st = St::Normal,
        _ => {}
      },
      St::Sq => match c {
        '\\' => { chars.next(); }
        '\'' => st = St::Normal,
        _ => {}
      },
    }
  }
  depth
}

// The editor asks whether the buffer is a complete entry when
// Enter is pressed:  while brackets stay open the buffer grows
// (Enter inserts a newline) rather than submitting.  A pasted
// block therefore lands as ONE editable buffer — bracketed paste
// hands it over whole, the validator judges the whole — instead
// of feeding a read_line loop line by line, where stray
// trailing lines of the paste turned into extra continuation
// prompts and a parse error the tester never typed.
#[derive(Completer, Helper, Highlighter, Hinter)]
struct Entry;

impl Validator for Entry {
  fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
    Ok(if open_depth(ctx.input()) > 0 {
      ValidationResult::Incomplete
    } else {
      ValidationResult::Valid(None)
    })
  }
}


async fn process_input(nockapp: &mut NockApp, input: &str) -> Result<String, Box<dyn Error>> {
  // Handle empty input
  if input.trim().is_empty() {
    return Ok(String::new());
  }

  let mut poke_slab = NounSlab::new();

  let str_atom = string_to_atom(&mut poke_slab, input)?;
  let command_noun = T(&mut poke_slab, &[D(tas!(b"command")), str_atom.as_noun()]);
  poke_slab.set_root(command_noun);

  match nockapp.poke(SystemWire.to_wire(), poke_slab).await {
    Ok(effects) => {
      let mut results = Vec::new();
      for (_i, effect) in effects.iter().enumerate() {
        let effect_noun = unsafe { effect.root() };
        // Only [tag cord] effects (the REPL's %markdown output) render;
        // skip anything else (e.g. a %crud goof from a blocked scry)
        // rather than panicking, so a peekContext miss/block is legible.
        if let Ok(cell) = effect_noun.as_cell() {
          let Ok(tail_atom) = cell.tail().as_atom() else { continue };
          let Ok(tail_string) = std::str::from_utf8(tail_atom.as_ne_bytes()) else { continue };
          results.push(tail_string.trim_end_matches('\0').to_string());
        }
      }
      Ok(results.last().cloned().unwrap_or_else(|| "(no answer: scry blocked or empty)".to_string()))
    }
    Err(_e) => {
      Ok("command failed".to_string())
    }
  }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
  // default to INFO: the vendored tracing falls back to TRACE,
  // which floods the session with gnort/mio debug output
  if std::env::var("RUST_LOG").is_err() {
    std::env::set_var("RUST_LOG", "info");
  }
  let cli = boot::default_boot_cli(false);
  boot::init_default_tracing(&cli);
  // The jam to boot comes as the first argument (default: the
  // committed jojo.jam) — so one host consumes any trap jam we
  // hand a tester, sealed kernels included.  The NockApp
  // instance is named by the jam's file stem, so different jams
  // never adopt each other's checkpointed state through +load.
  let jam_path = std::env::args().nth(1).unwrap_or_else(|| "jojo.jam".to_string());
  let kernel = fs::read(&jam_path).map_err(|e| format!("Failed to read {}: {}", jam_path, e))?;
  let instance = std::path::Path::new(&jam_path)
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("jojo")
    .to_string();

  let mut nockapp = boot::setup(&kernel, Some(cli), &[], &instance, None).await?;

  // Line editing, history and multiline entry all come from
  // rustyline:  Up/Down recall previous entries (a multiline
  // entry comes back whole, ready to edit and resubmit), Left/
  // Right/Home/End edit in place, Ctrl-C abandons the current
  // buffer, Ctrl-D on an empty line exits.  History persists in
  // .jojo_history beside the jam, one file per instance.
  let mut rl: Editor<Entry, _> = Editor::new()?;
  rl.set_helper(Some(Entry));
  let hist = format!(".{}_history", instance);
  let _ = rl.load_history(&hist);
  loop {
    match rl.readline("jojo> ") {
      Ok(entry) => {
        let chunk = entry.trim();
        if chunk.is_empty() {
          continue;
        }
        let _ = rl.add_history_entry(chunk);
        if chunk == "exit" || chunk == ":exit" || chunk == ":q" {
          break;
        }
        if let Ok(result) = process_input(&mut nockapp, chunk).await {
          println!("{}", result);
        }
      }
      Err(ReadlineError::Interrupted) => {
        println!("(cancelled)");
        continue;
      }
      Err(ReadlineError::Eof) => break,
      Err(error) => {
        println!("Error reading input: {}", error);
        break;
      }
    }
  }
  let _ = rl.save_history(&hist);
  // exit/EOF/Ctrl-D land here; exit the process rather than
  // returning, so the serf thread cannot keep the session alive
  println!("bye");
  std::process::exit(0);
}
