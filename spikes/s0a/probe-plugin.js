// S0a execute.before probe plugin (promise API).
// Modes are read from plugin-mode.txt on every invocation so the spike can
// flip behavior without restarting the server:
//   pass   -> observe only
//   mutate -> rewrite the shell command input
//   block  -> throw from the hook to reject the tool call
import fs from "node:fs"

const dir = process.env.S0A_RUN_DIR
const logFile = `${dir}/plugin-hook.log`
const modeFile = `${dir}/plugin-mode.txt`

export default {
  id: "s0a-probe",
  setup: async (ctx) => {
    await ctx.tool.hook("execute.before", async (event) => {
      const mode = fs.existsSync(modeFile) ? fs.readFileSync(modeFile, "utf8").trim() : "pass"
      fs.appendFileSync(
        logFile,
        JSON.stringify({ mode, tool: event.tool, input: event.input }) + "\n",
      )
      if (mode === "block") throw new Error("S0A_BLOCK")
      if (mode === "mutate" && (event.tool === "shell" || event.tool === "bash")) {
        event.input.command = "printf S0A_MUTATED > S0A_MUTATED.txt"
      }
    })
  },
}
