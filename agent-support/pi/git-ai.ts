import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { resolve, isAbsolute, join } from "node:path";

const GIT_AI_BIN = '__GIT_AI_BINARY_PATH__';
// Optional user-owned override file. Managed installer never writes it.
const OVERRIDE_CONFIG_PATH = join(homedir(), '.pi', 'agent', 'git-ai.override.json');

type CanonicalToolName = 'edit' | 'write' | 'replace' | 'rename' | 'bash';
type HookEventName = 'before_edit' | 'after_edit' | 'before_command' | 'after_command';

type ToolOverridePolicy = {
  kind: 'mutating';
  canonical: CanonicalToolName;
  filepath_fields: string[];
} | {
  kind: 'ignore';
};

type OverrideConfig = {
  version: 1;
  tools: Record<string, ToolOverridePolicy>;
};

const DEFAULT_TOOL_POLICIES: Record<string, ToolOverridePolicy> = {
  edit: {
    kind: 'mutating',
    canonical: 'edit',
    filepath_fields: ['path'],
  },
  write: {
    kind: 'mutating',
    canonical: 'write',
    filepath_fields: ['path'],
  },
};

function isCanonicalToolName(value: unknown): value is CanonicalToolName {
  return value === 'edit' || value === 'write' || value === 'replace' || value === 'rename' || value === 'bash';
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((entry) => typeof entry === 'string' && entry.trim().length > 0);
}

function normalizeToolPolicy(value: unknown): ToolOverridePolicy | undefined {
  if (typeof value !== 'object' || value === null) return undefined;
  const raw = value as Record<string, unknown>;

  if (raw.kind === 'ignore') {
    return { kind: 'ignore' };
  }

  if (
    raw.kind === 'mutating'
    && isCanonicalToolName(raw.canonical)
    && isStringArray(raw.filepath_fields)
  ) {
    return {
      kind: 'mutating',
      canonical: raw.canonical,
      filepath_fields: raw.filepath_fields,
    };
  }

  return undefined;
}

async function loadOverrideConfig(): Promise<OverrideConfig | undefined> {
  try {
    const raw = await readFile(OVERRIDE_CONFIG_PATH, 'utf8');
    const parsed = JSON.parse(raw);
    if (parsed?.version !== 1 || typeof parsed?.tools !== 'object' || parsed.tools === null) {
      return undefined;
    }

    const tools: Record<string, ToolOverridePolicy> = {};
    for (const [toolName, policy] of Object.entries(parsed.tools as Record<string, unknown>)) {
      if (typeof toolName !== 'string' || toolName.trim().length === 0) continue;
      const normalized = normalizeToolPolicy(policy);
      if (!normalized) continue;
      tools[toolName] = normalized;
    }

    return { version: 1, tools };
  } catch {
    return undefined;
  }
}

async function buildToolConfig() {
  const override = await loadOverrideConfig();
  const mergedPolicies = new Map<string, ToolOverridePolicy>(
    Object.entries({
      ...DEFAULT_TOOL_POLICIES,
      ...(override?.tools ?? {}),
    }),
  );

  return {
    toolPolicies: mergedPolicies,
  };
}

type MutatingCall = {
  toolNameRaw: string;
  toolName: CanonicalToolName;
  filepaths: string[];
};

type CheckpointPayload = {
  hook_event_name: HookEventName;
  session_id: string;
  cwd: string;
  tool_name: CanonicalToolName;
  tool_name_raw: string;
  tool_use_id?: string;
  edited_filepaths?: string[];
  command?: string;
};

function resolveAbsolutePath(cwd: string, pathValue: string): string {
  return isAbsolute(pathValue) ? pathValue : resolve(cwd, pathValue);
}

function normalizeFilepaths(cwd: string, rawPaths: unknown[]): string[] {
  const unique = new Set<string>();
  for (const rawPath of rawPaths) {
    if (typeof rawPath !== 'string' || rawPath.trim().length === 0) continue;
    unique.add(resolveAbsolutePath(cwd, rawPath));
  }
  return [...unique];
}

function extractDirectFilepaths(
  input: any,
  filepathFields: string[],
): string[] {
  const rawPaths: string[] = [];

  for (const field of filepathFields) {
    const value = input?.[field];
    if (typeof value === 'string') {
      rawPaths.push(value);
      continue;
    }
    if (Array.isArray(value)) {
      rawPaths.push(...value.filter((entry): entry is string => typeof entry === 'string'));
    }
  }

  return rawPaths;
}

function toMutatingCall(
  cwd: string,
  toolNameRaw: string,
  input: unknown,
  policy: ToolOverridePolicy,
): MutatingCall | undefined {
  if (policy.kind !== 'mutating') return undefined;

  const filepaths = normalizeFilepaths(
    cwd,
    extractDirectFilepaths(input as any, policy.filepath_fields),
  );
  if (filepaths.length === 0) return undefined;

  return { toolNameRaw, toolName: policy.canonical, filepaths };
}

function extractMutatingCalls(
  cwd: string,
  toolNameRaw: string,
  input: unknown,
  toolConfig: {
    toolPolicies: Map<string, ToolOverridePolicy>;
  },
): MutatingCall[] {
  const policy = toolConfig.toolPolicies.get(toolNameRaw);
  if (!policy || policy.kind !== 'mutating') {
    return [];
  }

  const direct = toMutatingCall(
    cwd,
    toolNameRaw,
    input,
    policy,
  );
  return direct ? [direct] : [];
}

async function runCheckpoint(payload: CheckpointPayload): Promise<void> {
  await new Promise<void>((resolvePromise) => {
    const child = spawn(GIT_AI_BIN, ['checkpoint', 'pi', '--hook-input', 'stdin'], {
      stdio: ['pipe', 'ignore', 'ignore'],
    });

    child.on('error', () => resolvePromise());
    child.on('close', () => resolvePromise());
    child.stdin.end(JSON.stringify(payload));
  });
}

export default function (pi: ExtensionAPI) {
  const pendingCalls = new Map<string, { sessionId: string; cwd: string; calls: MutatingCall[] }>();
  const pendingBashCalls = new Map<string, { sessionId: string; cwd: string; command?: string }>();
  const toolConfigPromise = buildToolConfig();

  pi.on('tool_call', async (event, ctx) => {
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;
    if (event.toolName === 'bash') {
      const command = typeof event.input?.command === 'string' ? event.input.command : undefined;
      pendingBashCalls.set(event.toolCallId, { sessionId, cwd: ctx.cwd, command });
      await runCheckpoint({
        hook_event_name: 'before_command', session_id: sessionId, cwd: ctx.cwd,
        tool_name: 'bash', tool_name_raw: event.toolName, tool_use_id: event.toolCallId, command,
      });
      return;
    }
    const calls = extractMutatingCalls(ctx.cwd, event.toolName, event.input, await toolConfigPromise);
    if (calls.length > 0) pendingCalls.set(event.toolCallId, { sessionId, cwd: ctx.cwd, calls });
  });

  pi.on('tool_result', async (event) => {
    const shell = pendingBashCalls.get(event.toolCallId);
    if (shell) {
      pendingBashCalls.delete(event.toolCallId);
      // A failed shell command can still have made writes before failing.
      await runCheckpoint({
        hook_event_name: 'after_command', session_id: shell.sessionId, cwd: shell.cwd,
        tool_name: 'bash', tool_name_raw: 'bash', tool_use_id: event.toolCallId, command: shell.command,
      });
      return;
    }
    const pending = pendingCalls.get(event.toolCallId);
    pendingCalls.delete(event.toolCallId);
    if (!pending || event.isError) return;
    for (const call of pending.calls) {
      await runCheckpoint({
        hook_event_name: 'after_edit', session_id: pending.sessionId, cwd: pending.cwd,
        tool_name: call.toolName, tool_name_raw: call.toolNameRaw,
        tool_use_id: event.toolCallId, edited_filepaths: call.filepaths,
      });
    }
  });
}
