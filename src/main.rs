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
use std::io::{self, Write};

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

  // Multiline entry: while the buffer's bracket depth stays
  // positive the prompt continues and lines accumulate; the
  // joined chunk pokes as ONE %command (the kernel's statement
  // test is the last byte after rtrim, so newlines inside are
  // fine).  A blank line during continuation cancels the buffer.
  let mut buf = String::new();
  loop {
    print!("{}", if buf.is_empty() { "jojo> " } else { "  ... " });
    io::stdout().flush().unwrap();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
      Ok(0) => {
        break;
      }
      Ok(_) => {
        let line = input.trim_end();
        if buf.is_empty() {
          let cmd = line.trim();
          if cmd == "exit" || cmd == ":exit" || cmd == ":q" {
            println!("bye");
            std::process::exit(0);
          }
        } else if line.trim().is_empty() {
          println!("(cancelled)");
          buf.clear();
          continue;
        }
        if !buf.is_empty() {
          buf.push('\n');
        }
        buf.push_str(line);
        if open_depth(&buf) > 0 {
          continue;
        }
        let chunk = std::mem::take(&mut buf);
        if let Ok(result) = process_input(&mut nockapp, chunk.trim()).await {
            println!("{}", result);
        }
      }
      Err(error) => {
        println!("Error reading input: {}", error);
        break;
      }
    }
  }
  // EOF (Ctrl+D) lands here; exit the process rather than
  // returning, so the serf thread cannot keep the session alive
  println!("bye");
  std::process::exit(0);
}
