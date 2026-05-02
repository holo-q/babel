//! Mermaid Architecture Diagram Generation
//!
//! `babel mmdc [dirpath] [out]?` - Generate holistic mermaid architecture diagrams
//!
//! This command fires a agent instance that:
//! 1. Maps the codebase broadly (tree, reading types)
//! 2. Dispatches up to 15 subagents to map different "regions"
//! 3. Aggregates results into a master mermaid document
//! 4. Optionally processes with `mmdc` to produce PNG
//!
//! The result is an exhaustive, accurate representation of the architecture
//! and how components are interconnected.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use tracing::instrument;

// ═══════════════════════════════════════════════════════════════════════════════
// Prompt Template
// ═══════════════════════════════════════════════════════════════════════════════

/// Master prompt for the architecture mapping agent
///
/// This prompt guides Claude through:
/// 1. Broad exploration (tree, types)
/// 2. Parallel subagent dispatch for regional mapping
/// 3. Aggregation and mermaid synthesis
const MMDC_PROMPT: &str = r#"**🗺️ ARCHITECTURE CARTOGRAPHER MODE**

You are mapping the architecture of a codebase to produce a comprehensive Mermaid diagram.
Your mission: create an **exhaustive, accurate, holistic** system map that represents:
- All major components/modules
- Data flow between components
- Dependencies and relationships
- System boundaries and interfaces

**Target Directory:** `{DIRPATH}`
**Output File:** `{OUTPUT_PATH}`

═══════════════════════════════════════════════════════════════════════════════
PHASE 1: BROAD RECONNAISSANCE
═══════════════════════════════════════════════════════════════════════════════

First, get a bird's-eye view of the codebase:

1. Run `ripmap {DIRPATH}` to get the architectural map
   - This surfaces load-bearing structures via PageRank on the symbol graph
   - Pay attention to [bridge] nodes (remove = disconnect), [api] nodes (external entry points)
   - Note coupling indicators (⇄ changes with) for implicit dependencies

2. Run `eza --tree -L 2 --git-ignore -I 'node_modules|__pycache__|.git|target|dist|build|vendor' {DIRPATH}` for directory layout

3. Read key files that reveal architecture (use absolute paths from {DIRPATH}):
   - README.md, ARCHITECTURE.md
   - Cargo.toml, package.json, pyproject.toml, meson.build (dependencies)
   - Main entry points (main.rs, index.ts, __main__.py)

After Phase 1, you should understand:
- The language(s) and frameworks used
- The load-bearing modules (bridges) vs peripheral code
- Key entry points and external interfaces ([api] nodes)
- Implicit coupling patterns

═══════════════════════════════════════════════════════════════════════════════
PHASE 2: PARALLEL REGIONAL MAPPING
═══════════════════════════════════════════════════════════════════════════════

Now dispatch **up to 15 parallel subagents** using the Task tool. Each subagent
should explore ONE region of the codebase and return a structured report.

For each major module/directory identified in Phase 1, spawn a worker with:

```
Prompt: "You are mapping ONE REGION of a codebase for architecture documentation.

Region: [specific directory/module path]
Parent context: [brief description of how this fits in the larger system]

Your task:
1. Run `ripmap [region path]` to get the structural map of this region
   - Note [bridge] files that connect this region to others
   - Note [api] files that expose interfaces
2. Read the key files identified by ripmap (focus on high-PageRank nodes)
3. Identify all:
   - Public interfaces/APIs this region exposes
   - Dependencies this region consumes (internal and external)
   - Data structures/types this region defines
   - Communication patterns (HTTP, IPC, events, direct calls)

Return a structured YAML report:
```yaml
region: [path]
purpose: [1-2 sentence description]
public_api:
  - name: [function/class/module name]
    description: [what it does]
dependencies:
  internal:
    - [module path]: [what for]
  external:
    - [package name]: [what for]
types:
  - name: [type name]
    kind: [struct/enum/interface/class]
    description: [purpose]
communicates_with:
  - target: [what it talks to]
    method: [HTTP/IPC/events/function call]
    direction: [inbound/outbound/bidirectional]
unresolved_complexity:  # optional - only if this region contains sub-systems that couldn't be mapped at this resolution
  - path: [sub-directory or module]
    reason: [why it warrants separate mapping]
```

Focus on architecture, not implementation details."
```

Distribute the workers across different regions. Use `run_in_background: true` and
`TaskOutput` to collect results efficiently.

**Recursive Zoom (if warranted):** After collecting initial results, check if any workers
reported `unresolved_complexity`. If critical sub-systems were flagged AND they represent
significant architectural weight, dispatch additional workers to map those regions.
Do not recurse purely due to size - only when structural complexity demands it.

═══════════════════════════════════════════════════════════════════════════════
PHASE 3: SYNTHESIS & MERMAID GENERATION
═══════════════════════════════════════════════════════════════════════════════

Once all workers complete, synthesize their reports into a **single comprehensive
Mermaid diagram**. Use these guidelines:

1. **Choose the right diagram type:**
   - `flowchart TD` for data/control flow
   - `graph LR` for component relationships
   - `sequenceDiagram` for interaction flows
   - `classDiagram` for type hierarchies
   - `C4Context` for high-level system boundaries

   For most codebases, a **flowchart** or **graph** with subgraphs works best.

2. **Use subgraphs to group related components:**
   ```mermaid
   flowchart TD
       subgraph Core
           A[Component A]
           B[Component B]
       end
       subgraph IO
           C[Network]
           D[Storage]
       end
       A --> C
       B --> D
   ```

3. **Label edges with relationship types:**
   ```mermaid
   A -->|"HTTP API"| B
   B -->|"events"| C
   B -->|"queries"| D
   ```

4. **Include all major components** - don't simplify for readability, be exhaustive.

5. **Write the diagram to the output file using the Write tool.**

═══════════════════════════════════════════════════════════════════════════════
PHASE 4: RENDER (optional)
═══════════════════════════════════════════════════════════════════════════════

After writing the .mmd file, run:
```bash
mmdc -i "{OUTPUT_PATH}" -o "{OUTPUT_PATH_PNG}" -b transparent
```

If mmdc isn't available, note this in your output and the user can run it manually.

═══════════════════════════════════════════════════════════════════════════════
FINAL OUTPUT
═══════════════════════════════════════════════════════════════════════════════

When complete, report:
1. The path to the generated .mmd file
2. The path to the generated .png file (if rendered)
3. A brief summary of what was mapped

**Begin Phase 1 now. Map the terrain.**"#;

// ═══════════════════════════════════════════════════════════════════════════════
// Command Handler
// ═══════════════════════════════════════════════════════════════════════════════

/// Handle the `babel mmdc` command
///
/// Fires a agent instance with the architecture mapping prompt. The Claude
/// session does the actual work of exploring, dispatching subagents, and
/// synthesizing the mermaid diagram.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_mmdc(
    dirpath: PathBuf,
    out: Option<PathBuf>,
    fire: bool,
    verbose: bool,
) -> Result<()> {
    // Resolve dirpath to absolute
    let dirpath = dirpath
        .canonicalize()
        .context("Failed to resolve directory path")?;

    if !dirpath.is_dir() {
        return Err(anyhow!("Not a directory: {}", dirpath.display()));
    }

    // Determine output path
    let output_path = match out {
        Some(p) if p.is_dir() => p.join("architecture.mmd"),
        Some(p) => {
            // Ensure parent directory exists
            if let Some(parent) = p.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent).context("Failed to create output directory")?;
                }
            }
            p
        }
        None => dirpath.join("architecture.mmd"),
    };

    // Ensure output has .mmd extension
    let output_path = if output_path.extension().and_then(|e| e.to_str()) != Some("mmd") {
        output_path.with_extension("mmd")
    } else {
        output_path
    };

    let output_path_png = output_path.with_extension("png");

    // Build the prompt
    let prompt = MMDC_PROMPT
        .replace("{DIRPATH}", &dirpath.display().to_string())
        .replace("{OUTPUT_PATH}", &output_path.display().to_string())
        .replace("{OUTPUT_PATH_PNG}", &output_path_png.display().to_string());

    tracing::info!(
        dirpath = %dirpath.display(),
        output = %output_path.display(),
        fire,
        verbose,
        "Launching architecture mapper"
    );

    eprintln!("🗺️  Architecture Mapper");
    eprintln!("   Source: {}", dirpath.display());
    eprintln!("   Output: {}", output_path.display());

    if fire {
        // Fire-and-forget mode using existing fire infrastructure
        eprintln!("   Mode: fire-and-forget");
        launch_fire(&dirpath, &prompt).await?;
        eprintln!("   ✓ Fired in background");
        eprintln!();
        eprintln!("   Monitor with: babel fire-ls");
        eprintln!("   Check output: {}", output_path.display());
    } else {
        // Default: run Claude with -p, stream to stdout, then cat the output file
        // This allows: babel mmdc . > diagram.mmd or piping to other tools
        if verbose {
            eprintln!("   Mode: verbose (full transcript to stdout)");
            eprintln!();
            launch_print(&dirpath, &prompt).await?;
        } else {
            eprintln!("   Mode: print (mermaid output to stdout)");
            eprintln!();
            // Run Claude silently, then output just the mermaid file
            launch_print_quiet(&dirpath, &prompt).await?;

            // Cat the output file to stdout
            if output_path.exists() {
                let content =
                    std::fs::read_to_string(&output_path).context("Failed to read output file")?;
                println!("{}", content);
            } else {
                eprintln!("   ⚠ Output file not created: {}", output_path.display());
            }
        }
    }

    Ok(())
}

/// Launch Claude in print mode (-p) with output streamed to stdout (verbose)
///
/// This runs Claude Code in non-interactive mode where all output goes to stdout.
/// Uses --output-format stream-json for real-time streaming without buffering.
async fn launch_print(cwd: &PathBuf, prompt: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    // Run Claude with -p and stream-json for unbuffered real-time output
    // --verbose is required for stream-json
    let mut child = Command::new("claude")
        .args(["-p", "--verbose", "--output-format", "stream-json"])
        .arg(prompt)
        .current_dir(cwd)
        .env("SHELL", "/usr/bin/bash")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit()) // Stream to our stdout
        .stderr(Stdio::inherit()) // Stream to our stderr
        .spawn()
        .context("Failed to spawn claude")?;

    // Wait for completion
    let status = child.wait().context("Failed to wait for claude")?;

    if !status.success() {
        return Err(anyhow!("Claude exited with status: {}", status));
    }

    Ok(())
}

/// Launch Claude in print mode (-p) with output going to stderr (quiet)
///
/// Claude's transcript goes to stderr so stdout is clean for the final output.
/// This enables: babel mmdc . > diagram.mmd
async fn launch_print_quiet(cwd: &PathBuf, prompt: &str) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    // Run Claude with -p and stream-json, pipe stdout to our stderr
    // --verbose is required for stream-json
    let mut child = Command::new("claude")
        .args(["-p", "--verbose", "--output-format", "stream-json"])
        .arg(prompt)
        .current_dir(cwd)
        .env("SHELL", "/usr/bin/bash")
        .stdin(Stdio::null())
        .stdout(Stdio::piped()) // Capture stdout
        .stderr(Stdio::inherit()) // Pass through stderr
        .spawn()
        .context("Failed to spawn claude")?;

    // Stream Claude's stdout to our stderr (so user sees progress)
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        let mut stderr = std::io::stderr();
        for line in reader.lines() {
            if let Ok(line) = line {
                let _ = writeln!(stderr, "{}", line);
            }
        }
    }

    // Wait for completion
    let status = child.wait().context("Failed to wait for claude")?;

    if !status.success() {
        return Err(anyhow!("Claude exited with status: {}", status));
    }

    Ok(())
}

/// Launch Claude in fire-and-forget mode
async fn launch_fire(cwd: &PathBuf, prompt: &str) -> Result<()> {
    use babel::fire::{track_task, FiredTask};
    use std::process::{Command, Stdio};

    // Write prompt to temp file for safe passing
    let prompt_file = std::env::temp_dir().join(format!("babel-mmdc-{}.txt", std::process::id()));
    std::fs::write(&prompt_file, prompt)?;

    // Create launcher script that reads prompt and cleans up
    let script_file = prompt_file.with_extension("sh");
    std::fs::write(
        &script_file,
        format!(
            r#"#!/bin/sh
PROMPT=$(cat '{}')
rm -f '{}' '{}'
exec claude "$PROMPT"
"#,
            prompt_file.display(),
            prompt_file.display(),
            script_file.display()
        ),
    )?;

    // Launch detached with kitty
    let mut cmd = Command::new("kitty");
    cmd.args(["@", "launch"])
        .args(["--type", "os-window"])
        .args(["--cwd", &cwd.to_string_lossy()])
        .args(["--env", "SHELL=/usr/bin/bash"])
        .arg("--")
        .arg("sh")
        .arg(&script_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    // Target main socket if available
    if let Some(socket) = babel::kitty::main_socket() {
        cmd.args(["--to", &socket]);
    }

    let child = cmd.spawn().context("Failed to spawn kitty")?;
    let pid = child.id();

    // Track the fired task
    let task = FiredTask {
        task_id: FiredTask::new_id(),
        pid,
        prompt_preview: "🗺️ Architecture mapping...".to_string(),
        workdir: cwd.clone(),
        ambient_sound: None,
    };
    track_task(&task)?;

    Ok(())
}
