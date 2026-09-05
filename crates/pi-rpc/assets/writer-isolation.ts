/**
 * writer-isolation.ts — R27 mutating 子代理 worktree 强制。
 *
 * 钉死的 pi-subagents-lite 只把 worktree_path 当可选参数；本扩展在 Agent 的
 * tool_call 阶段强制：
 *   1. mutating（非 Explore）必须落在独立 linked worktree；
 *   2. 缺省或指向父 checkout 时自动 `git worktree add` 并改写 event.input；
 *   3. 同一 canonical path 最多一个活跃 writer，冲突则 block。
 *
 * 租约生命周期：
 *   - 前台 Agent：tool_result 时释放；
 *   - 后台 Agent：tool_result 只是派发成功，必须等到 subagent-result /
 *     StopAgent / session_shutdown 才释放（否则第二个 Agent 会并发写入）。
 *
 * 清理仍归 Rust Manager（remove_worktree 含 reparse 扫描）；这里只持进程内租约。
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, realpathSync } from "node:fs";
import { basename, dirname, join, resolve, sep } from "node:path";

const READ_ONLY_TYPES = new Set(["explore"]);

/** toolCallId → canonical worktree path（派发中 / 前台运行中） */
const leaseByToolCall = new Map<string, string>();
/** 后台 agentId → canonical worktree path（派发成功后、完成前） */
const leaseByAgentId = new Map<string, string>();
/** canonical worktree path → holder（`tool:<id>` 或 `agent:<id>`） */
const writerByPath = new Map<string, string>();

function canonicalPath(path: string): string {
  try {
    const real = realpathSync(path);
    return process.platform === "win32" ? real.toLowerCase() : real;
  } catch {
    const resolved = resolve(path);
    return process.platform === "win32" ? resolved.toLowerCase() : resolved;
  }
}

function isReadOnlyAgent(agent: string | undefined): boolean {
  const key = (agent ?? "general-purpose").trim().toLowerCase();
  return READ_ONLY_TYPES.has(key);
}

function git(cwd: string, args: string[]): string {
  return execFileSync("git", args, {
    cwd,
    encoding: "utf8",
    timeout: 60_000,
    windowsHide: true,
  }).trim();
}

/** 比较 git toplevel：同一工作树 → 不独立；linked worktree 的 toplevel 不同 → 独立。 */
function isIndependent(parentCwd: string, worktreePath: string): boolean {
  try {
    const parentTop = canonicalPath(git(parentCwd, ["rev-parse", "--show-toplevel"]));
    const candidateTop = canonicalPath(
      git(worktreePath, ["rev-parse", "--show-toplevel"]),
    );
    return parentTop !== candidateTop;
  } catch {
    // 候选不是 git 目录时退回路径前缀判断（fail closed：同前缀视为不独立）。
    const parent = canonicalPath(parentCwd);
    const candidate = canonicalPath(worktreePath);
    if (parent === candidate) return false;
    const prefix = parent.endsWith(sep) ? parent : parent + sep;
    if (candidate.startsWith(prefix)) return false;
    return true;
  }
}

function sanitizeBranchSegment(label: string): string {
  return label
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .toLowerCase()
    .slice(0, 40);
}

/**
 * 与 pi_data::add_worktree 同构：`<repoParent>/<repoName>-worktrees/<branchDir>`。
 */
function allocateWriterWorktree(parentCwd: string, toolCallId: string): string {
  const repoRoot = git(parentCwd, ["rev-parse", "--show-toplevel"]);
  const name = basename(repoRoot);
  const base = join(dirname(repoRoot), `${name}-worktrees`);
  if (!existsSync(base)) mkdirSync(base, { recursive: true });

  const stamp = Date.now().toString(36);
  const short = sanitizeBranchSegment(toolCallId).slice(0, 12) || "agent";
  const branch = `gpui-pi/writer-${short}-${stamp}`;
  const directoryName = branch.replace(/[\\/:*?"<>|]+/g, "-");
  const target = join(base, directoryName);

  let branchExists = false;
  try {
    git(repoRoot, ["rev-parse", "--verify", "--quiet", `refs/heads/${branch}`]);
    branchExists = true;
  } catch {
    branchExists = false;
  }

  if (branchExists) {
    git(repoRoot, ["worktree", "add", "--", target, branch]);
  } else {
    git(repoRoot, ["worktree", "add", "-b", branch, "--", target]);
  }
  return canonicalPath(target);
}

function releaseHolder(holder: string): void {
  for (const [path, current] of writerByPath) {
    if (current === holder) {
      writerByPath.delete(path);
    }
  }
}

function releaseToolCall(toolCallId: string | undefined): void {
  if (!toolCallId) return;
  const path = leaseByToolCall.get(toolCallId);
  leaseByToolCall.delete(toolCallId);
  if (path && writerByPath.get(path) === `tool:${toolCallId}`) {
    writerByPath.delete(path);
  }
}

function releaseAgent(agentId: string | undefined): void {
  if (!agentId) return;
  const path = leaseByAgentId.get(agentId);
  leaseByAgentId.delete(agentId);
  if (path && writerByPath.get(path) === `agent:${agentId}`) {
    writerByPath.delete(path);
  }
}

function extractAgentId(details: unknown, content: unknown): string | undefined {
  if (details && typeof details === "object") {
    const record = details as Record<string, unknown>;
    for (const key of ["agentId", "agent_id", "id"]) {
      const value = record[key];
      if (typeof value === "string" && value.trim() !== "") return value.trim();
    }
  }
  if (typeof content === "string") {
    const match = content.match(/Agent ID:\s*([A-Za-z0-9_-]+)/);
    if (match?.[1]) return match[1];
  }
  if (Array.isArray(content)) {
    for (const part of content) {
      if (part && typeof part === "object" && "text" in part) {
        const text = (part as { text?: unknown }).text;
        if (typeof text === "string") {
          const match = text.match(/Agent ID:\s*([A-Za-z0-9_-]+)/);
          if (match?.[1]) return match[1];
        }
      }
    }
  }
  return undefined;
}

export default function writerIsolation(pi: ExtensionAPI): void {
  pi.on("tool_call", async (event, ctx: ExtensionContext) => {
    if (event.toolName === "StopAgent") {
      const input = event.input as Record<string, unknown>;
      const agentId =
        typeof input.agent_id === "string"
          ? input.agent_id
          : typeof input.agentId === "string"
            ? input.agentId
            : undefined;
      releaseAgent(agentId);
      return;
    }

    if (event.toolName !== "Agent") return;

    const input = event.input as Record<string, unknown>;
    const agent = typeof input.agent === "string" ? input.agent : "general-purpose";
    if (isReadOnlyAgent(agent)) return;

    const parentCwd = ctx.cwd;
    let raw =
      typeof input.worktree_path === "string" ? input.worktree_path.trim() : "";
    if (raw === "" || !isIndependent(parentCwd, raw)) {
      try {
        raw = allocateWriterWorktree(parentCwd, event.toolCallId ?? `t${Date.now()}`);
        input.worktree_path = raw;
      } catch (err: unknown) {
        const message = err instanceof Error ? err.message : String(err);
        return {
          block: true,
          reason: `无法为 mutating 子代理分配独立 worktree：${message}`,
        };
      }
    }

    const key = canonicalPath(raw);
    const holder = writerByPath.get(key);
    const selfHolder = event.toolCallId ? `tool:${event.toolCallId}` : undefined;
    if (holder && holder !== selfHolder) {
      return {
        block: true,
        reason: `同一 worktree 已有活跃 writer（${holder}）：${key}`,
      };
    }
    if (event.toolCallId) {
      releaseToolCall(event.toolCallId);
      writerByPath.set(key, `tool:${event.toolCallId}`);
      leaseByToolCall.set(event.toolCallId, key);
    }
  });

  pi.on("tool_result", (event) => {
    if (event.toolName === "StopAgent") {
      const agentId = extractAgentId(event.details, event.content);
      releaseAgent(agentId);
      return;
    }
    if (event.toolName !== "Agent") return;

    const input = event.input as Record<string, unknown> | undefined;
    const background =
      input?.run_in_background === true || input?.runInBackground === true;
    const toolCallId = event.toolCallId;
    const path = toolCallId ? leaseByToolCall.get(toolCallId) : undefined;

    if (event.isError || !background) {
      releaseToolCall(toolCallId);
      return;
    }

    // 后台派发成功：租约从 toolCall 转挂到 agentId，直到 subagent-result / StopAgent。
    const agentId = extractAgentId(event.details, event.content);
    if (agentId && path) {
      leaseByToolCall.delete(toolCallId!);
      leaseByAgentId.set(agentId, path);
      writerByPath.set(path, `agent:${agentId}`);
      return;
    }
    // 解析不出 agentId 时保守持有 tool 租约，避免误放行并发 writer。
  });

  pi.on("message_end", (event) => {
    const message = event.message as {
      role?: string;
      customType?: string;
      content?: unknown;
      details?: unknown;
    };
    if (message?.role !== "customMessage" && message?.role !== "custom") {
      // pi 自定义消息 role 可能是 custom / 带 customType 的 assistant 旁路；
      // 以 customType 为准。
    }
    const customType =
      typeof (message as { customType?: unknown })?.customType === "string"
        ? (message as { customType: string }).customType
        : undefined;
    if (customType !== "subagent-result") return;
    const agentId = extractAgentId(
      (message as { details?: unknown }).details,
      (message as { content?: unknown }).content,
    );
    releaseAgent(agentId);
  });

  pi.on("session_shutdown", () => {
    leaseByToolCall.clear();
    leaseByAgentId.clear();
    writerByPath.clear();
  });
}
