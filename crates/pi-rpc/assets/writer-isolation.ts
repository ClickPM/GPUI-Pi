/**
 * writer-isolation.ts — R27 mutating 子代理 worktree 强制。
 *
 * 钉死的 pi-subagents-lite 只把 worktree_path 当可选参数；本扩展在 Agent 的
 * tool_call 阶段强制：
 *   1. mutating（非 Explore）必须落在独立 linked worktree；
 *   2. 缺省或指向父 checkout 时自动 `git worktree add` 并改写 event.input；
 *   3. 同一 canonical path 最多一个活跃 writer，冲突则 block。
 *
 * 清理仍归 Rust Manager（remove_worktree 含 reparse 扫描）；这里只持进程内租约。
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, realpathSync } from "node:fs";
import { basename, dirname, join, resolve, sep } from "node:path";

const READ_ONLY_TYPES = new Set(["explore"]);

/** toolCallId → canonical worktree path */
const leaseByToolCall = new Map<string, string>();
/** canonical worktree path → toolCallId */
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

function isIndependent(parentCwd: string, worktreePath: string): boolean {
  const parent = canonicalPath(parentCwd);
  const candidate = canonicalPath(worktreePath);
  if (parent === candidate) return false;
  const prefix = parent.endsWith(sep) ? parent : parent + sep;
  if (candidate.startsWith(prefix)) return false;
  return true;
}

function git(cwd: string, args: string[]): string {
  return execFileSync("git", args, {
    cwd,
    encoding: "utf8",
    timeout: 60_000,
    windowsHide: true,
  }).trim();
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

  // 分支不存在时从 HEAD 创建；已存在则直接挂 worktree。
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

function releaseToolCall(toolCallId: string | undefined): void {
  if (!toolCallId) return;
  const path = leaseByToolCall.get(toolCallId);
  if (!path) return;
  leaseByToolCall.delete(toolCallId);
  if (writerByPath.get(path) === toolCallId) {
    writerByPath.delete(path);
  }
}

export default function writerIsolation(pi: ExtensionAPI): void {
  pi.on("tool_call", async (event, ctx: ExtensionContext) => {
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
    if (holder && holder !== event.toolCallId) {
      return {
        block: true,
        reason: `同一 worktree 已有活跃 writer（toolCall ${holder}）：${key}`,
      };
    }
    if (event.toolCallId) {
      // 同一 toolCall 重入时先释放旧 path
      releaseToolCall(event.toolCallId);
      writerByPath.set(key, event.toolCallId);
      leaseByToolCall.set(event.toolCallId, key);
    }
  });

  pi.on("tool_result", (event) => {
    if (event.toolName !== "Agent") return;
    releaseToolCall(event.toolCallId);
  });
}
