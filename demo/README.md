# Demo task: "Copy sample files to output folder"

A harmless two-minute walkthrough that shows the whole AgentTracker flow. Nothing outside the demo
folder is touched.

## What's in here
| Path | Role in the task |
|---|---|
| `input/shopping_list.txt`, `input/meeting_notes.txt`, `input/ideas.txt` | **Protected** — must not change |
| `expected_output/` | **Allowed to write** — copies go here |
| `.` (the demo folder) | **Working folder** |
| `task.json` | The task definition (title, folders, 5 checks) |

The checks:
1. **Originals unchanged** — the three input files are byte-for-byte identical afterwards.
2. **Required outputs exist** — `expected_output/` contains all three files.
3. **Every input made it to the output** — each input's content appears in the output.
4. **Folder looks right** *(optional)* — the output only contains `.txt` files.
5. **Changes stayed in bounds** — nothing changed outside `expected_output/`.

## Walkthrough
1. Open AgentTracker. On **Home**, click **Try the demo**. AgentTracker copies these sample files
   into its own data folder (shown in a message) and creates the task. You land on **Live Task**.
2. Click **📸 Take the "before" snapshot**. AgentTracker fingerprints every file in the folders.
3. Click **▶️ Start**. Now do the work — either:
   - **with an agent**: open the demo folder in Claude Code (connected under **Connections**) and
     ask: *"Copy every file from input into expected_output. Don't change anything in input."*
     Its tool calls appear under *What the agent reported*; or
   - **by hand**: copy the three files from `input` to `expected_output` in Explorer.
4. Click **🔍 The agent is done — check it**. You'll see 🎉 *Everything checks out* on **Results**.

## Try breaking it
Run the demo again (each run gets a fresh folder) and, before checking:
- forget one file → *Required outputs exist* **fails**;
- edit `input/ideas.txt` → *Originals unchanged* and *Changes stayed in bounds* **fail**;
- open `input/ideas.txt` in a program that locks it while checking → **Unable to verify**.

AgentTracker never undoes anything — it only reports what it found.
