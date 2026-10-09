# Agalma

> An always-running software factory. It evaluates, builds, and ships its target — and never stops.

Named after *Agalma*, a genus of siphonophore: colonial deep-sea organisms built from specialized parts that are continuously replaced, so the colony itself persists indefinitely. Agalma the factory borrows the model, not the vocabulary.

## The loop

```
recon → design → build → verify → ship → iterate
```

The loop never terminates. Each pass evaluates the current state of the target, decides the next task, and executes it.

## Roles

- **orchestrator** — runs the loop, assigns work
- **researcher** — external recon and scanning
- **planner** — breaks the target into tasks
- **builder** — implements
- **evaluator** — reviews output against acceptance criteria
- **tester** — runs verification
- **guard** — safety, policy, and sandbox gate
- **deployer** — ships releases

## Status

Early. Architecture next.
