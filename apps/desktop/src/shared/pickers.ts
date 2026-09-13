//! Composer pickers — the TypeScript port of `crates/ui/src/pickers.rs`'s
//! pure resolution logic. These derivations must not diverge between the
//! GPUI and Electron surfaces; each function names its Rust source.

import type {
  Chat,
  ChatConfig,
  HarnessDescriptor,
  HarnessId,
  Model,
  ReasoningLevel,
  SandboxLevel,
} from "./types";

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// `DraftConfig` — picks made on the new-chat canvas before the first send.
export interface DraftConfig {
  harness?: HarnessId;
  model?: string;
  reasoning?: ReasoningLevel;
  /** option id → choice id (only non-defaults are meaningful). */
  modelOptions: Record<string, unknown>;
  /** The picked ref (base branch in NewWorktree mode). */
  branch?: string;
  /** Where the new session runs (the t3code env-mode). */
  checkout: CheckoutKind;
}

export type CheckoutKind =
  | { kind: "local" }
  | { kind: "newWorktree" };

export const CHECKOUT_LOCAL: CheckoutKind = { kind: "local" };
export const CHECKOUT_NEW_WORKTREE: CheckoutKind = { kind: "newWorktree" };

/// `CheckoutPlan` — the resolved on-send checkout action the composer
/// consumes (Pickers::checkout_plan).
export type CheckoutPlan =
  | { kind: "currentCheckout"; branch?: string }
  | { kind: "reuseWorktree"; path: string; branch: string }
  | { kind: "newWorktree"; base?: string };

/// `ResolvedRunConfig` — the fully-resolved run configuration the composer
/// sends: concrete harness, model and reasoning (never a "default"
/// passthrough once the catalog is loaded), plus explicit non-default
/// option picks.
export interface ResolvedRunConfig {
  harness?: HarnessId;
  model?: string;
  reasoning?: ReasoningLevel;
  modelOptions: Record<string, unknown>;
}

/// `ResolvedRunConfig::chat_config` — the `ChatConfig` recorded on
/// `Mutate createChat` / `setChatConfig` (needs a known harness).
export function chatConfigOf(
  resolved: ResolvedRunConfig,
  sandbox: SandboxLevel,
): ChatConfig | null {
  if (!resolved.harness) return null;
  return {
    harness: resolved.harness,
    model: resolved.model ?? null,
    reasoning: resolved.reasoning ?? null,
    modelOptions: resolved.modelOptions,
    sandbox,
  };
}

// ---------------------------------------------------------------------------
// Default resolution (no "Default" placeholders — a concrete pick always)
// ---------------------------------------------------------------------------

/// `default_model` — the harness's default model: the first catalog row.
export function defaultModel(models: Model[]): Model | undefined {
  return models[0];
}

/// `default_reasoning` — High when the ladder offers it, else Medium, else
/// the ladder's first entry. `undefined` only for ladder-less models.
export function defaultReasoning(
  ladder: ReasoningLevel[],
): ReasoningLevel | undefined {
  if (ladder.includes("high")) return "high";
  if (ladder.includes("medium")) return "medium";
  return ladder[0];
}

/// `clamp_reasoning` — keep a picked/remembered level when the ladder lists
/// it, else fall to the model's default (never a stale or foreign level).
export function clampReasoning(
  level: ReasoningLevel | undefined | null,
  ladder: ReasoningLevel[],
): ReasoningLevel | undefined {
  if (level && ladder.includes(level)) return level;
  return defaultReasoning(ladder);
}

// ---------------------------------------------------------------------------
// Labels + traits summary
// ---------------------------------------------------------------------------

/// `reasoning_label`.
export function reasoningLabel(level: ReasoningLevel): string {
  switch (level) {
    case "off":
      return "Off";
    case "minimal":
      return "Minimal";
    case "low":
      return "Low";
    case "medium":
      return "Medium";
    case "high":
      return "High";
    case "xhigh":
      return "X-High";
    case "max":
      return "Max";
    case "ultra":
      return "Ultra";
    case "ultracode":
      return "Ultracode";
    case "ultrathink":
      return "Ultrathink";
  }
}

/// `traits_summary` — the effective reasoning level plus every model
/// option's effective choice (explicit pick when offered, else the
/// option's default), joined with " · ". `null` only when the model has
/// nothing to describe (no ladder, no options).
export function traitsSummary(
  model: Model | undefined,
  reasoning: ReasoningLevel | undefined,
  selections: Record<string, unknown>,
): string | null {
  const parts: string[] = [];
  if (reasoning) parts.push(reasoningLabel(reasoning));
  if (model) {
    for (const option of model.options) {
      const picked = selections[option.id];
      const choiceId =
        typeof picked === "string" &&
        option.choices.some((c) => c.id === picked)
          ? picked
          : option.defaultChoice;
      const choice = option.choices.find((c) => c.id === choiceId);
      if (choice) parts.push(choice.label);
    }
  }
  return parts.length === 0 ? null : parts.join(" · ");
}

/// `traits_customized` — whether any trait departs from its default.
export function traitsCustomized(
  model: Model | undefined,
  reasoning: ReasoningLevel | undefined,
  ladder: ReasoningLevel[],
  selections: Record<string, unknown>,
): boolean {
  if (reasoning !== defaultReasoning(ladder)) return true;
  return (
    model?.options.some((option) => {
      const picked = selections[option.id];
      return (
        typeof picked === "string" &&
        picked !== option.defaultChoice &&
        option.choices.some((c) => c.id === picked)
      );
    }) ?? false
  );
}

// ---------------------------------------------------------------------------
// Harness catalog filtering (visible/offered + descriptor_enabled)
// ---------------------------------------------------------------------------

/// `descriptor_enabled` — a descriptor's effective enabled flag; `None`
/// (catalog from an engine predating the setting) falls back to detection.
export function descriptorEnabled(d: HarnessDescriptor): boolean {
  return d.enabled ?? (d.installed && d.id !== "mock");
}

/// `visible_harnesses` — the mock harness never lists (the Electron client
/// has no ZERON_HARNESS env knob; mock stays hidden unless it is literally
/// all the catalog holds — a dev build with no real harness registered).
export function visibleHarnesses(
  list: HarnessDescriptor[],
): HarnessDescriptor[] {
  const real = list.filter((d) => d.id !== "mock");
  return real.length === 0 ? [...list] : real;
}

/// `offered_harnesses` — what the composer actually offers: the visible set
/// narrowed to enabled AND installed. NO fallback: a catalog where nothing
/// is both enabled and installed offers nothing.
export function offeredHarnesses(
  list: HarnessDescriptor[],
): HarnessDescriptor[] {
  return visibleHarnesses(list).filter((d) => d.installed && descriptorEnabled(d));
}

// ---------------------------------------------------------------------------
// Effective-config resolution (the store feeds; pure)
// ---------------------------------------------------------------------------

export interface ResolveInput {
  /** Canvas draft picks (new chat) — `undefined` fields fall through. */
  draft?: Partial<DraftConfig>;
  /** The open chat row (existing chat). */
  chat?: Chat;
  /** Models already fetched for the effective harness, if any. */
  models?: Model[];
  /** Harness catalog rows for fallbacks. */
  harnesses: HarnessDescriptor[];
  /** Sticky last-used picks (composer defaults). */
  defaults?: {
    harness?: HarnessId;
    model?: string;
    reasoning?: ReasoningLevel;
  };
}

/// `effective_harness` — picked → chat config → remembered default → first
/// offered.
export function effectiveHarness(input: ResolveInput): HarnessId | undefined {
  if (input.draft?.harness) return input.draft.harness;
  if (input.chat?.config?.harness) return input.chat.config.harness;
  if (input.defaults?.harness) {
    const offered =
      input.harnesses.length === 0 ||
      offeredHarnesses(input.harnesses).some(
        (d) => d.id === input.defaults!.harness,
      );
    if (offered) return input.defaults.harness;
  }
  return offeredHarnesses(input.harnesses)[0]?.id;
}

/// `effective_model_id` — draft pick → chat config → remembered default.
export function effectiveModelId(input: ResolveInput): string | undefined {
  if (input.draft?.model) return input.draft.model;
  if (input.chat?.config?.model) return input.chat.config.model;
  const harness = effectiveHarness(input);
  return harness ? input.defaults?.model : undefined;
}

/// `selected_model` — concrete from the moment the catalog loads: the
/// effective id when the list still offers it, else the harness default.
export function selectedModel(input: ResolveInput): Model | undefined {
  const models = input.models;
  if (!models || models.length === 0) return undefined;
  const id = effectiveModelId(input);
  if (id) return models.find((m) => m.id === id) ?? defaultModel(models);
  return defaultModel(models);
}

/// The traits ladder: model levels, falling back to the harness's
/// advertised ladder (`trait_ladder`).
export function traitLadder(input: ResolveInput): ReasoningLevel[] {
  const model = selectedModel(input);
  if (model && model.reasoningLevels.length > 0) return model.reasoningLevels;
  const harness = effectiveHarness(input);
  return (
    input.harnesses.find((d) => d.id === harness)?.reasoningLevels ?? []
  );
}

/// `effective_reasoning` — always concrete once the model is known: draft →
/// chat config → remembered default, clamped to the selected model's ladder.
export function effectiveReasoning(
  input: ResolveInput,
): ReasoningLevel | undefined {
  const explicit =
    input.draft?.reasoning ?? input.chat?.config?.reasoning ?? input.defaults?.reasoning;
  if (!selectedModel(input)) {
    // Catalog not loaded yet: show the explicit value as-is; it resolves to
    // a concrete level on load.
    return explicit;
  }
  return clampReasoning(explicit, traitLadder(input));
}

/// The explicit (non-default) option picks: the chat's persisted selections
/// for existing chats, the draft's for the new-chat canvas.
export function explicitOptions(input: ResolveInput): Record<string, unknown> {
  return input.chat?.config?.modelOptions ?? input.draft?.modelOptions ?? {};
}

/// `resolved` — the fully-resolved config the composer threads into the Run
/// request and `Mutate createChat`/`setChatConfig`.
export function resolveRunConfig(input: ResolveInput): ResolvedRunConfig {
  return {
    harness: effectiveHarness(input),
    model:
      selectedModel(input)?.id ??
      effectiveModelId(input),
    reasoning: effectiveReasoning(input),
    modelOptions: explicitOptions(input),
  };
}

// ---------------------------------------------------------------------------
// Engine version gating (state.rs device_version_at_least)
// ---------------------------------------------------------------------------

/// `version_triple` — parse "0.2.12" (suffix junk tolerated) into a triple.
export function versionTriple(v: string | null | undefined): [number, number, number] | null {
  if (!v) return null;
  const m = v.trim().match(/^(\d+)\.(\d+)\.(\d+)/);
  if (!m) return null;
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

function cmpTriple(a: [number, number, number], b: [number, number, number]): number {
  return a[0] - b[0] || a[1] - b[1] || a[2] - b[2];
}

/// `device_version_at_least` — unknown/unstamped versions are conservatively
/// false (feature gates fall back to the legacy path).
export function deviceVersionAtLeast(
  version: string | null | undefined,
  min: [number, number, number],
): boolean {
  const v = versionTriple(version);
  return v !== null && cmpTriple(v, min) >= 0;
}

/// `QUEUED_ATTACHMENTS_MIN` — engines at or above this understand
/// `pending://` attachment refs and QueueCommand `transfers`.
export const QUEUED_ATTACHMENTS_MIN: [number, number, number] = [0, 2, 12];
