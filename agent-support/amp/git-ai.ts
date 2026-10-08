// @i-know-the-amp-plugin-api-is-wip-and-very-experimental-right-now
// Required by Amp: this exact marker must remain the first line in the plugin file.
import type { PluginAPI, ToolCallEvent } from '@ampcode/plugin'
import { spawn } from 'child_process'
import { isAbsolute, resolve } from 'path'
import { fileURLToPath } from 'url'

// Absolute path to git-ai binary, replaced at install time by `git-ai install-hooks`
const GIT_AI_BIN = '__GIT_AI_BINARY_PATH__'

type PendingToolCall = {
	cwd: string
	tool: string
	threadID: string
	command?: string
	editedFilepaths: string[]
}

function uniqueNonEmptyPaths(paths: string[]): string[] {
	const deduped = new Set<string>()
	for (const path of paths) {
		const trimmed = path.trim()
		if (trimmed) {
			deduped.add(trimmed)
		}
	}
	return Array.from(deduped)
}

function pathFromFileUri(uri: string): string | null {
	try {
		const parsed = new URL(uri)
		if (parsed.protocol !== 'file:') {
			return null
		}
		return fileURLToPath(parsed)
	} catch {
		return null
	}
}

function filesFromInput(input: Record<string, unknown>): string[] {
	const paths: string[] = []
	for (const key of ['path', 'filePath', 'file_path']) {
		const value = input[key]
		if (typeof value === 'string') {
			paths.push(value)
		}
	}

	for (const key of ['paths', 'filePaths', 'filepaths']) {
		const value = input[key]
		if (!Array.isArray(value)) {
			continue
		}
		for (const item of value) {
			if (typeof item === 'string') {
				paths.push(item)
			}
		}
	}

	return uniqueNonEmptyPaths(paths)
}

function filesFromToolCall(amp: PluginAPI, event: ToolCallEvent): string[] {
	const fromHelper = amp.helpers.filesModifiedByToolCall(event)
	const helperPaths =
		fromHelper
			?.map((uri) => pathFromFileUri(uri.toString()))
			.filter((value): value is string => value !== null) ?? []

	if (helperPaths.length > 0) {
		return uniqueNonEmptyPaths(helperPaths)
	}

	return filesFromInput(event.input)
}

export default function ampGitAiPlugin(amp: PluginAPI) {
	const pendingCalls = new Map<string, PendingToolCall>()
	let gitAiInstalledPromise: Promise<boolean> | null = null

	const runProcess = (
		command: string,
		args: string[],
		options?: { stdin?: string },
	): Promise<{ exitCode: number; stdout: string; stderr: string }> => {
		return new Promise((resolve, reject) => {
			const child = spawn(command, args, {
				stdio: ['pipe', 'pipe', 'pipe'],
			})

			let stdout = ''
			let stderr = ''

			child.stdout.on('data', (chunk: unknown) => {
				stdout += chunk.toString()
			})
			child.stderr.on('data', (chunk: unknown) => {
				stderr += chunk.toString()
			})
			child.on('error', reject)
			child.on('close', (code) => {
				resolve({
					exitCode: code ?? -1,
					stdout,
					stderr,
				})
			})

			if (typeof options?.stdin === 'string') {
				child.stdin.write(options.stdin)
			}
			child.stdin.end()
		})
	}

	const ensureGitAiInstalled = async () => {
		if (!gitAiInstalledPromise) {
			gitAiInstalledPromise = (async () => {
				try {
					const result = await runProcess(GIT_AI_BIN, ['--version'])
					return result.exitCode === 0
				} catch {
					return false
				}
			})()
		}
		return gitAiInstalledPromise
	}

	const runCheckpoint = async (ctx: { logger: PluginAPI['logger'] }, payload: Record<string, unknown>) => {
		try {
			const hookInput = JSON.stringify(payload)
			const result = await runProcess(
				GIT_AI_BIN,
				['checkpoint', 'amp', '--hook-input', 'stdin'],
				{ stdin: hookInput },
			)

			const stderr = result.stderr.trim()
			const shouldLogStderr =
				result.exitCode !== 0 ||
				/Failed to find any git repositories|preset error|Error: --hook-input/i.test(stderr)
			if (shouldLogStderr) {
				ctx.logger.log(
					`[git-ai] checkpoint amp failed (exit=${result.exitCode})${
						stderr ? `: ${stderr}` : ''
					}`,
				)
			}
		} catch (error) {
			ctx.logger.log('[git-ai] checkpoint amp failed', String(error))
		}
	}

	amp.on('tool.call', async (event, ctx) => {
		const threadID = event.thread?.id
		if (!threadID || !(await ensureGitAiInstalled())) return { action: 'allow' as const }
		const isBash = event.tool === 'Bash' || event.tool === 'shell_command'
		const cwd = typeof event.input.cwd === 'string' ? resolve(event.input.cwd) : process.cwd()
		const editedFilepaths = filesFromToolCall(amp, event).map(path => isAbsolute(path) ? path : resolve(cwd, path))
		if (!isBash && editedFilepaths.length === 0) return { action: 'allow' as const }
		const command = typeof event.input.command === 'string' ? event.input.command : undefined
		const pending: PendingToolCall = { cwd, tool: event.tool, threadID, command, editedFilepaths }
		pendingCalls.set(event.toolUseID, pending)
		if (isBash) await runCheckpoint({ logger: ctx.logger }, {
			hook_event_name: 'PreToolUse', tool_use_id: event.toolUseID, tool_name: pending.tool,
			thread_id: pending.threadID, cwd: pending.cwd, tool_input: { command },
		})
		return { action: 'allow' as const }
	})

	amp.on('tool.result', async (event, ctx) => {
		const pending = pendingCalls.get(event.toolUseID)
		pendingCalls.delete(event.toolUseID)
		if (!pending) return
		const isBash = pending.tool === 'Bash' || pending.tool === 'shell_command'
		if (!isBash && event.status !== 'done') return
		await runCheckpoint({ logger: ctx.logger }, {
			hook_event_name: 'PostToolUse', tool_use_id: event.toolUseID, tool_name: pending.tool,
			thread_id: pending.threadID, cwd: pending.cwd,
			...(isBash ? { tool_input: { command: pending.command } } : { edited_filepaths: pending.editedFilepaths }),
		})
	})
}
