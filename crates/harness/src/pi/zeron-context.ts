/**
 * Zeron companion extension for pi.
 *
 * Measures what each turn actually sends to the model — system prompt, tool
 * definitions, skills, context files, conversation messages — and appends the
 * estimate to the session file as a custom entry. Zeron's pi harness reads
 * that entry to show a context-window breakdown in its usage card.
 *
 * Constraints:
 * - Passive: custom entries never participate in LLM context, and nothing here
 *   prompts, aborts, or otherwise steers the agent.
 * - Best-effort: every handler swallows its own errors. A failure must cost
 *   the breakdown, never the turn — pi surfaces a throwing handler to the
 *   client as an extension error.
 * - Cheap: char-count estimation (chars/4), written only when the estimate
 *   changes, so long sessions don't accumulate entries per streaming request.
 *
 * Timing: the `context` event fires BEFORE each LLM request, when pi's
 * reported usage is still the previous turn's — so prompt-side parts are
 * measured there, while the authoritative total is read from
 * `ctx.getContextUsage()` at `agent_settled`, after the response landed.
 */

const ENTRY_TYPE = "zeron:context-usage";
const ENTRY_VERSION = 1;
const CHARS_PER_TOKEN = 4;

interface MeasuredInputs {
	systemPromptChars: number;
	toolsChars: number;
	skillsChars: number;
	contextFilesChars: number;
	messagesChars: number;
}

function estimateTokens(chars: number): number {
	return Math.ceil(chars / CHARS_PER_TOKEN);
}

/** JSON length of a message, with base64 payloads (images) collapsed first. */
function messageChars(message: unknown): number {
	try {
		const json = JSON.stringify(message);
		if (!json) return 0;
		return json.replace(/[A-Za-z0-9+/=]{512,}/g, "omitted").length;
	} catch {
		return 0;
	}
}

function measureInputs(pi: any, event: any, ctx: any, promptOptions: any): MeasuredInputs {
	const prompt: string = ctx.getSystemPrompt?.() ?? "";
	const contextFilesChars = (promptOptions?.contextFiles ?? []).reduce(
		(sum: number, file: any) => sum + (file?.content?.length ?? 0) + (file?.path?.length ?? 0),
		0,
	);
	const skillsChars = (promptOptions?.skills ?? []).reduce(
		(sum: number, skill: any) => sum + (skill?.name?.length ?? 0) + (skill?.description?.length ?? 0),
		0,
	);
	// Tool snippets are bullets inside the system prompt; their schema JSON is
	// not, so count both but keep the subtraction below consistent.
	const snippets = Object.values(promptOptions?.toolSnippets ?? {}).reduce(
		(sum: number, snippet: any) => sum + String(snippet ?? "").length,
		0,
	);
	const basePromptChars = Math.max(0, prompt.length - contextFilesChars - skillsChars - snippets);

	const active: Set<string> = new Set(pi.getActiveTools?.() ?? []);
	const toolsChars = (pi.getAllTools?.() ?? []).reduce((sum: number, tool: any) => {
		if (!active.has(tool?.name)) return sum;
		let schema = 0;
		try {
			schema = JSON.stringify(tool?.parameters ?? {}).length;
		} catch {
			schema = 0;
		}
		return sum + (tool?.name?.length ?? 0) + (tool?.description?.length ?? 0) + schema;
	}, 0);

	const messagesChars = (event?.messages ?? []).reduce(
		(sum: number, message: any) => sum + messageChars(message),
		0,
	);

	return { systemPromptChars: basePromptChars, toolsChars, skillsChars, contextFilesChars, messagesChars };
}

function reportedUsage(ctx: any): { totalTokens: number | null; contextWindow: number | null } {
	const reported = ctx?.getContextUsage?.();
	const positive = (value: unknown): number | null =>
		typeof value === "number" && Number.isFinite(value) && value > 0 ? Math.round(value) : null;
	return { totalTokens: positive(reported?.tokens), contextWindow: positive(reported?.contextWindow) };
}

export default function (pi: any) {
	let promptOptions: any;
	let lastWritten: string | undefined;
	let lastInputs: MeasuredInputs | undefined;

	pi.on("before_agent_start", (event: any) => {
		try {
			if (event?.systemPromptOptions) promptOptions = event.systemPromptOptions;
		} catch {}
	});

	pi.on("context", (event: any, ctx: any) => {
		try {
			lastInputs = measureInputs(pi, event, ctx, promptOptions);
		} catch {
			lastInputs = undefined;
		}
	});

	pi.on("agent_settled", (_event: any, ctx: any) => {
		try {
			const inputs = lastInputs;
			if (!inputs) return;
			const systemPrompt = estimateTokens(inputs.systemPromptChars);
			const tools = estimateTokens(inputs.toolsChars);
			const skills = estimateTokens(inputs.skillsChars);
			const contextFiles = estimateTokens(inputs.contextFilesChars);
			const estimated = estimateTokens(inputs.messagesChars);
			// Fold estimation drift into the messages residual so the parts sum
			// to pi's authoritative whole (fall back to the raw estimate while
			// no usage has been reported yet).
			const { totalTokens, contextWindow } = reportedUsage(ctx);
			const messages =
				totalTokens !== null
					? Math.max(0, totalTokens - (systemPrompt + tools + skills + contextFiles))
					: estimated;
			const entry = { v: ENTRY_VERSION, systemPrompt, tools, skills, contextFiles, messages, totalTokens, contextWindow };
			const key = JSON.stringify(entry);
			if (key === lastWritten) return;
			pi.appendEntry(ENTRY_TYPE, entry);
			lastWritten = key;
		} catch {}
	});
}
