use nockapp::kernel::boot;
use nockapp::NockApp;
use nockapp::noun::slab::{NockJammer, NounSlab};
use nockapp::wire::{SystemWire, Wire};
use nockapp::{AtomExt};
use nockvm::noun::{Atom, NounAllocator, D, T};
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


// What one poke answers:  the text to print (the last rendered
// effect) and, when the kernel sent a [%prompt cord] effect, the
// prompt the line editor shows from the next line on.
struct Answer { text: String, prompt: Option<String> }

// What a poke came back as.  A REFUSED command and a DEAD KERNEL are
// different events and the REPL has to tell them apart:  the first is
// the session working, the second means no later poke can succeed
// until the kernel is rebooted.  Answering "command failed" to both
// made a dead session indistinguishable from a bad line, and every
// line after the crash printed the same three words (jojo#17).
enum Outcome {
  Answered(Answer),
  // the serf thread is gone;  the string is the error it died with
  KernelDown(String),
}

async fn process_input(nockapp: &mut NockApp<NockJammer>, tag: u64, input: &str) -> Result<Outcome, Box<dyn Error>> {
  // Handle empty input
  if input.trim().is_empty() {
    return Ok(Outcome::Answered(Answer { text: String::new(), prompt: None }));
  }

  let mut poke_slab = NounSlab::new();

  let str_atom = string_to_atom(&mut poke_slab, input)?;
  let command_noun = T(&mut poke_slab, &[D(tag), str_atom.as_noun()]);
  poke_slab.set_root(command_noun);

  match nockapp.poke(SystemWire.to_wire(), poke_slab).await {
    Ok(effects) => {
      let mut results = Vec::new();
      let mut prompt = None;
      for effect in effects.iter() {
        let space = effect.noun_space();
        let effect_noun = unsafe { effect.root() }.in_space(&space);
        // Only [tag cord] effects render:  [%prompt cord] sets the
        // prompt for the lines that follow (a kernel with modes, such
        // as Atalante's apple prompts, announces its mode this way);
        // any other tag (the REPL's %markdown output) is text, and the
        // last one is the answer.  Skip anything else (e.g. a %crud
        // goof from a blocked scry) rather than panicking, so a
        // peekContext miss/block is legible.
        if let Ok(cell) = effect_noun.as_cell() {
          let Ok(head_atom) = cell.head().noun().in_space(&space).as_atom() else { continue };
          let Ok(tail_atom) = cell.tail().noun().in_space(&space).as_atom() else { continue };
          let Ok(tail_bytes) = tail_atom.to_bytes_until_nul() else { continue };
          let Ok(head_bytes) = head_atom.to_bytes_until_nul() else { continue };
          let text = String::from_utf8_lossy(&tail_bytes).to_string();
          let is_prompt = head_bytes == b"prompt";
          if is_prompt { prompt = Some(text); } else { results.push(text); }
        }
      }
      Ok(Outcome::Answered(Answer {
        text: results.last().cloned().unwrap_or_else(|| "(no answer: scry blocked or empty)".to_string()),
        prompt,
      }))
    }
    // The error was DISCARDED here until 2026-11-10, which is the
    // whole of jojo#17:  a NockStack exhaustion panics the serf, the
    // poke comes back Err, and so does every poke after it, each
    // answering a bare "command failed" that names neither the cause
    // nor the fact that the kernel is gone.
    //
    // WHICH ERRORS MEAN THE SERF IS GONE, measured rather than
    // guessed.  Reading nockapp suggests CrownError::Unknown
    // ("Serf thread panicked: ...") and JoinError, which is what the
    // Stop/join path reports.  A poke does NOT take that path:  the
    // serf thread is already unwinding, so what comes back is a
    // CLOSED CHANNEL.  Reproduced on a NockStack exhaustion --- the
    // fatal poke answers "oneshot channel error" (its reply sender
    // dropped) and every poke after it "Serf MPSC error" (the action
    // channel is closed).  A classifier built on the first two alone
    // matched neither and never fired.
    //
    // All four mean reboot.  Both channel errors are closed-channel
    // conditions rather than transient ones --- nockapp's driver
    // documents MPSCSendError as "the channel is closed" --- so
    // neither can fire against a live kernel.  Anything else is one
    // poke failing while the kernel still answers, so it prints and
    // the session carries on.
    Err(e) => {
      let msg = e.to_string();
      let dead = msg.contains("Serf MPSC error")
        || msg.contains("oneshot channel error")
        || msg.contains("Serf thread panicked")
        || msg.contains("join error");
      if dead {
        Ok(Outcome::KernelDown(msg))
      } else {
        Ok(Outcome::Answered(Answer { text: format!("command failed: {}", msg), prompt: None }))
      }
    }
  }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
  // default to INFO: the vendored tracing falls back to TRACE,
  // which floods the session with gnort/mio debug output
  let checking = std::env::args().any(|a| a == "--check");
  if std::env::var("RUST_LOG").is_err() {
    std::env::set_var("RUST_LOG", if checking { "error" } else { "info" });
  }
  let cli = boot::default_boot_cli(false);
  boot::init_default_tracing(&cli);
  // The jam to boot comes as the first argument (default: the
  // committed jojo.jam) — so one host consumes any trap jam we
  // hand a tester, sealed kernels included.  The NockApp
  // instance is named by the jam's file stem, so different jams
  // never adopt each other's checkpointed state through +load.
  //
  // --check FILE is the one-shot agent lane: poke the kernel's
  // %check cause with the file's text, print the JSON answer
  // (the compiler's +chkj cord, schema pinned in the jock
  // corpus), exit 0 if it typechecks, 1 on a refusal.  The warm
  // kernel is what makes this sub-second: the checkpoint carries
  // the cold state, so the check runs jetted — a bare one-shot
  // evaluator cannot do this (the sealed-kernel finding).
  //
  // --prompt STR is the prompt the session opens with (default
  // `jojo> `); a kernel changes it afterwards with a [%prompt cord]
  // effect.
  let mut jam_arg: Option<String> = None;
  let mut check_arg: Option<String> = None;
  let mut prompt_arg: Option<String> = None;
  let mut argv = std::env::args().skip(1);
  while let Some(a) = argv.next() {
    if a == "--check" {
      match argv.next() {
        Some(f) => check_arg = Some(f),
        None => { eprintln!("jojo: --check needs a source file"); std::process::exit(64); }
      }
    } else if a == "--prompt" {
      match argv.next() {
        Some(p) => prompt_arg = Some(p),
        None => { eprintln!("jojo: --prompt needs a string"); std::process::exit(64); }
      }
    } else if jam_arg.is_none() {
      jam_arg = Some(a);
    } else {
      eprintln!("jojo: unexpected argument {}", a);
      std::process::exit(64);
    }
  }
  let jam_path = jam_arg.unwrap_or_else(|| "jojo.jam".to_string());
  let kernel = fs::read(&jam_path).map_err(|e| format!("Failed to read {}: {}", jam_path, e))?;
  let instance = std::path::Path::new(&jam_path)
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("jojo")
    .to_string();

  // The hot state:  nockvm's own jets, the hoon-138 float-door jets and
  // the /lib/lagoon jets (crates/lagoon-jets), so a kernel's @rd
  // arithmetic and its Lagoon %i754 arms run jetted.  HOON_FLOAT_JET_DISABLE=1
  // leaves the float doors to the Nock; NOCK_TEST_JETS=path,... makes the
  // runtime run jet and Nock side by side and bail on any mismatch.
  let hot = lagoon_jets::hot_state();
  let mut nockapp = boot::setup::<NockJammer>(&kernel, cli, &hot, &instance, None).await?;

  if let Some(src_path) = check_arg {
    let src = fs::read_to_string(&src_path)
      .map_err(|e| format!("Failed to read {}: {}", src_path, e))?;
    let out = match process_input(&mut nockapp, tas!(b"check"), src.trim_end()).await? {
      Outcome::Answered(a) => a.text,
      // The kernel dying is not a verdict ABOUT THE SOURCE, and a
      // one-shot lane must not let it read as one:  exit 70 rather
      // than 1, so a harness can tell "this file does not check"
      // from "the checker died" (jojo#17).
      Outcome::KernelDown(err) => {
        eprintln!("jojo: kernel failure during --check, no verdict: {}", err);
        std::process::exit(70);
      }
    };
    println!("{}", out);
    // the verdict comes from the JSON's leading status field, not a
    // substring scan: +chkj emits {"status":... first and +jesc
    // escapes report text, but anchoring at the START means an
    // embedded '"status":"ok"' inside a quoted source line can
    // never flip the exit code even if escaping ever regresses
    let ok = out.trim_start().starts_with("{\"status\":\"ok\"");
    std::process::exit(if ok { 0 } else { 1 });
  }

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
  let mut prompt = prompt_arg.unwrap_or_else(|| "jojo> ".to_string());
  // CONSECUTIVE kernel restarts, and a ceiling on them.  A poke that
  // kills the serf can be replayed by the restarted kernel --- state
  // recovery is nockapp's business, not jojo's --- and without a
  // ceiling that is an infinite restart loop with a loud banner each
  // time.  Three attempts, then exit 70 and let a driver decide.
  const MAX_RESTARTS: u32 = 3;
  let mut restarts: u32 = 0;
  loop {
    match rl.readline(&prompt) {
      Ok(entry) => {
        let chunk = entry.trim();
        if chunk.is_empty() {
          continue;
        }
        let _ = rl.add_history_entry(chunk);
        if chunk == "exit" || chunk == ":exit" || chunk == ":q" {
          break;
        }
        match process_input(&mut nockapp, tas!(b"command"), chunk).await {
          Ok(Outcome::Answered(answer)) => {
            // a poke that answered means the kernel is healthy, so the
            // restart budget is spent on CONSECUTIVE failures only
            restarts = 0;
            // an empty answer (a statement, an assignment) prints no line
            if !answer.text.is_empty() {
              println!("{}", answer.text);
            }
            if let Some(p) = answer.prompt {
              prompt = p;
            }
          }
          Ok(Outcome::KernelDown(err)) => {
            // LOUDLY, and on stderr:  the session just lost its state,
            // which the user has to know before they read the next
            // answer as continuous with the last (jojo#17).
            eprintln!();
            eprintln!("*** KERNEL FAILURE: the serf is gone. {}", err);
            restarts += 1;
            if restarts > MAX_RESTARTS {
              eprintln!("*** {} consecutive restarts; giving up rather than looping.", MAX_RESTARTS);
              eprintln!("*** If the command that killed it is reproducible, that is a kernel bug worth filing.");
              let _ = rl.save_history(&hist);
              std::process::exit(70);
            }
            eprintln!("*** restarting from {} (attempt {}/{})", jam_path, restarts, MAX_RESTARTS);
            match boot::setup::<NockJammer>(&kernel, boot::default_boot_cli(false), &hot, &instance, None).await {
              // WHAT SURVIVES, measured on a NockStack exhaustion
              // rather than assumed.  The restart boots the same jam
              // through the same path as startup, so nockapp recovers
              // the session from the data directory:  a `let` and a
              // `func` defined before the crash both still answer
              // afterwards.  And the poke that killed the kernel is
              // NOT replayed --- it panics before the event commits,
              // so it never reaches the log the restart recovers.
              //
              // Neither is a jojo guarantee:  both are nockapp's
              // recovery behaviour, and a run with no durable data
              // directory would come back empty.  So the banner says
              // what to re-check rather than promising a state.
              Ok(fresh) => {
                nockapp = fresh;
                eprintln!("*** kernel restarted. Definitions are recovered where the data");
                eprintln!("*** directory has them; re-check anything defined just before.");
              }
              // Nothing further can work, so say so and leave with a
              // distinct code rather than pretending to be a prompt.
              Err(e) => {
                eprintln!("*** restart FAILED, the session cannot continue: {}", e);
                let _ = rl.save_history(&hist);
                std::process::exit(70);
              }
            }
          }
          Err(e) => {
            eprintln!("jojo: {}", e);
          }
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
