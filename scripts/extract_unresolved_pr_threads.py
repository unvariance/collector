#!/usr/bin/env python3
import argparse
import base64
import json
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from typing import Dict, Iterable, List, Optional, Tuple

try:
    from urllib.request import Request, urlopen
    from urllib.error import HTTPError, URLError
except ImportError:
    print("This script requires Python 3.", file=sys.stderr)
    sys.exit(1)


GITHUB_API = "https://api.github.com"
GITHUB_GRAPHQL = "https://api.github.com/graphql"


@dataclass
class Anchor:
    path: str
    side: str  # "RIGHT" or "LEFT"
    commit_sha: str
    start_line: int
    end_line: int


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Extract unresolved code review conversations on a GitHub Pull Request, "
            "including code context (±N lines) and all messages, and write Markdown to stdout."
        ),
        epilog=(
            "If no PR is specified, the script attempts to infer the current PR from the local repo using `gh pr view`, "
            "or GitHub Actions environment variables."
        ),
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--pr",
        help="PR URL like https://github.com/<owner>/<repo>/pull/<number>",
    )
    parser.add_argument("--owner", help="GitHub repository owner")
    parser.add_argument("--repo", help="GitHub repository name")
    parser.add_argument("--pull", type=int, help="Pull request number")
    # Output is written to stdout so it can be piped or redirected.
    parser.add_argument(
        "-c",
        "--context",
        type=int,
        default=5,
        help="Number of context lines around the commented region",
    )
    parser.add_argument(
        "--token",
        help=(
            "GitHub token. If omitted, uses GITHUB_TOKEN or GH_TOKEN env vars, "
            "or falls back to `gh auth token` if the GitHub CLI is configured."
        ),
    )
    return parser.parse_args()


def parse_pr_url(url: str) -> Tuple[str, str, int]:
    m = re.match(r"^https?://github.com/([^/]+)/([^/]+)/pull/(\d+)(?:/.*)?$", url)
    if not m:
        raise ValueError("Invalid PR URL. Expected https://github.com/<owner>/<repo>/pull/<number>")
    owner, repo, num = m.group(1), m.group(2), int(m.group(3))
    return owner, repo, num


def infer_pr(token: Optional[str]) -> Optional[Tuple[str, str, int]]:
    # Try GitHub CLI first
    gh = shutil.which("gh")
    if gh:
        try:
            out = subprocess.check_output(
                [gh, "pr", "view", "--json", "number,url"],
                stderr=subprocess.STDOUT,
                text=True,
            )
            obj = json.loads(out)
            if isinstance(obj, dict) and obj.get("number") and obj.get("url"):
                owner, repo, num = parse_pr_url(obj["url"])
                return owner, repo, num
        except Exception:
            pass
    # GitHub Actions environment fallback
    gh_repo = os.getenv("GITHUB_REPOSITORY")  # owner/repo
    gh_ref = os.getenv("GITHUB_REF") or ""
    if gh_repo:
        try:
            owner, repo = gh_repo.split("/", 1)
            m = re.match(r"refs/pull/(\d+)/(?:head|merge)", gh_ref)
            if m:
                return owner, repo, int(m.group(1))
            # If we have branch context, try to resolve PR by branch via REST (if token)
            branch = os.getenv("GITHUB_HEAD_REF") or os.getenv("GITHUB_REF_NAME")
            if token and branch:
                url = f"{GITHUB_API}/repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=open&per_page=100"
                try:
                    status, _hdrs, data = http_get(url, token)
                    if status == 200:
                        arr = json.loads(data.decode("utf-8"))
                        if isinstance(arr, list) and arr:
                            return owner, repo, int(arr[0].get("number"))
                        # try state=all
                        url_all = f"{GITHUB_API}/repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=all&per_page=100"
                        status2, _h2, data2 = http_get(url_all, token)
                        if status2 == 200:
                            arr2 = json.loads(data2.decode("utf-8"))
                            if isinstance(arr2, list) and arr2:
                                return owner, repo, int(arr2[0].get("number"))
                except Exception:
                    pass
        except Exception:
            pass
    return None


def http_get(url: str, token: Optional[str], accept: Optional[str] = None) -> Tuple[int, Dict[str, str], bytes]:
    headers = {
        "Accept": accept or "application/vnd.github+json",
        "User-Agent": "unresolved-pr-threads-script",
    }
    t = token or os.getenv("GITHUB_TOKEN")
    if t:
        headers["Authorization"] = f"Bearer {t}"
    req = Request(url, headers=headers, method="GET")
    try:
        with urlopen(req) as resp:
            status = resp.getcode()
            hdrs = {k.lower(): v for k, v in resp.headers.items()}
            data = resp.read()
            return status, hdrs, data
    except HTTPError as e:
        content = e.read() if hasattr(e, 'read') else b''
        raise RuntimeError(f"HTTP error {e.code} for GET {url}: {content!r}")
    except URLError as e:
        raise RuntimeError(f"Network error for GET {url}: {e}")


def http_post_json(url: str, token: Optional[str], payload: Dict) -> Tuple[int, Dict[str, str], Dict]:
    headers = {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "User-Agent": "unresolved-pr-threads-script",
    }
    t = token or os.getenv("GITHUB_TOKEN")
    if t:
        headers["Authorization"] = f"Bearer {t}"
    data = json.dumps(payload).encode("utf-8")
    req = Request(url, headers=headers, data=data, method="POST")
    try:
        with urlopen(req) as resp:
            status = resp.getcode()
            hdrs = {k.lower(): v for k, v in resp.headers.items()}
            raw = resp.read()
            obj = json.loads(raw.decode("utf-8"))
            return status, hdrs, obj
    except HTTPError as e:
        content = e.read() if hasattr(e, 'read') else b''
        raise RuntimeError(f"HTTP error {e.code} for POST {url}: {content!r}")
    except URLError as e:
        raise RuntimeError(f"Network error for POST {url}: {e}")


def parse_link_header(value: Optional[str]) -> Dict[str, str]:
    links: Dict[str, str] = {}
    if not value:
        return links
    for part in value.split(','):
        m = re.match(r'\s*<([^>]+)>;\s*rel="([^"]+)"', part)
        if m:
            links[m.group(2)] = m.group(1)
    return links


def fetch_all_pages(url: str, token: Optional[str]) -> List[Dict]:
    items: List[Dict] = []
    next_url = url
    while next_url:
        status, headers, data = http_get(next_url, token)
        if status != 200:
            raise RuntimeError(f"Unexpected status {status} for {next_url}")
        chunk = json.loads(data.decode("utf-8"))
        if isinstance(chunk, list):
            items.extend(chunk)
        else:
            raise RuntimeError(f"Expected list response for paginated GET {next_url}")
        links = parse_link_header(headers.get("link"))
        next_url = links.get("next")
        # avoid rapid-fire requests if many pages
        if next_url:
            time.sleep(0.05)
    return items


def list_unresolved_threads_graphql(owner: str, repo: str, pull: int, token: Optional[str]) -> List[Dict]:
    # Fetch unresolved review threads via GraphQL, with pagination for threads and comments.
    query = (
        "query($owner:String!, $name:String!, $number:Int!, $after:String) { "
        "repository(owner:$owner, name:$name) { "
        "pullRequest(number:$number) { "
        "reviewThreads(first: 50, after: $after) { "
        "nodes { id isResolved isOutdated comments(first: 100) { nodes { "
        "id databaseId body createdAt author { login } diffHunk path position originalPosition "
        "line originalLine startLine originalStartLine commit { oid } originalCommit { oid } "
        "} pageInfo { hasNextPage endCursor } } } "
        "pageInfo { hasNextPage endCursor } } } } }"
    )

    unresolved: List[Dict] = []
    after: Optional[str] = None
    while True:
        variables = {"owner": owner, "name": repo, "number": pull, "after": after}
        status, _hdrs, obj = http_post_json(GITHUB_GRAPHQL, token, {"query": query, "variables": variables})
        if status != 200:
            raise RuntimeError(f"GraphQL status {status}")
        if "errors" in obj and obj["errors"]:
            raise RuntimeError(f"GraphQL errors: {obj['errors']}")
        pr = (((obj.get("data") or {}).get("repository") or {}).get("pullRequest") or {})
        rt = (pr.get("reviewThreads") or {})
        nodes = rt.get("nodes") or []
        for node in nodes:
            if node.get("isResolved"):
                continue
            # Ensure we page comments if needed
            comments = (node.get("comments") or {})
            comment_nodes = comments.get("nodes") or []
            c_has_next = (comments.get("pageInfo") or {}).get("hasNextPage")
            c_after = (comments.get("pageInfo") or {}).get("endCursor")
            # If comments are paginated, fetch remaining
            while c_has_next:
                c_query = (
                    "query($threadId:ID!, $after:String) { "
                    "node(id:$threadId) { ... on PullRequestReviewThread { comments(first:100, after:$after) { nodes { "
                    "id databaseId body createdAt author { login } diffHunk path position originalPosition "
                    "line originalLine startLine originalStartLine commit { oid } originalCommit { oid } "
                    "} pageInfo { hasNextPage endCursor } } } } }"
                )
                status2, _h2, obj2 = http_post_json(
                    GITHUB_GRAPHQL,
                    token,
                    {
                        "query": c_query,
                        "variables": {
                            "threadId": node.get("id"),
                            "after": c_after,
                        },
                    },
                )
                if status2 != 200 or ("errors" in obj2 and obj2["errors"]):
                    raise RuntimeError(f"GraphQL error while paging comments: {obj2.get('errors')}")
                rt2 = (((obj2.get("data") or {}).get("node") or {}))
                prt = rt2.get("comments") or {}
                more_nodes = (prt.get("nodes") or [])
                comment_nodes.extend(more_nodes)
                pi = prt.get("pageInfo") or {}
                c_has_next = pi.get("hasNextPage")
                c_after = pi.get("endCursor")

            unresolved.append({
                "id": node.get("id"),
                "is_outdated": node.get("isOutdated"),
                "comments": comment_nodes,
            })

        has_next = (rt.get("pageInfo") or {}).get("hasNextPage")
        after = (rt.get("pageInfo") or {}).get("endCursor")
        if not has_next:
            break
        time.sleep(0.05)

    return unresolved


def choose_anchor_from_comments(comments: List[Dict]) -> Optional[Anchor]:
    for c in comments:
        path = c.get("path")
        if not path:
            continue
        # Determine based on which side fields are present (GraphQL names)
        line = c.get("line")
        start_line = c.get("startLine")
        orig_line = c.get("originalLine")
        orig_start_line = c.get("originalStartLine")
        if line is not None or start_line is not None:
            side = "RIGHT"
            commit_obj = c.get("commit") or {}
            commit_sha = commit_obj.get("oid")
            end_line = line if line is not None else start_line
            s_line = start_line if start_line is not None else line
        else:
            side = "LEFT"
            commit_obj = c.get("originalCommit") or {}
            commit_sha = commit_obj.get("oid")
            end_line = orig_line if orig_line is not None else orig_start_line
            s_line = orig_start_line if orig_start_line is not None else orig_line
        if end_line is None and s_line is not None:
            end_line = s_line
        if s_line is None and end_line is not None:
            s_line = end_line
        if path and commit_sha and s_line and end_line:
            return Anchor(
                path=path,
                side=side,
                commit_sha=str(commit_sha),
                start_line=int(s_line),
                end_line=int(end_line),
            )
    return None


def get_language_from_path(path: str) -> str:
    ext = os.path.splitext(path)[1].lower()
    mapping = {
        ".py": "python",
        ".js": "javascript",
        ".ts": "typescript",
        ".tsx": "tsx",
        ".jsx": "jsx",
        ".go": "go",
        ".rs": "rust",
        ".rb": "ruby",
        ".java": "java",
        ".kt": "kotlin",
        ".swift": "swift",
        ".cs": "csharp",
        ".c": "c",
        ".h": "c",
        ".cpp": "cpp",
        ".cc": "cpp",
        ".cxx": "cpp",
        ".hpp": "cpp",
        ".m": "objectivec",
        ".mm": "objectivecpp",
        ".php": "php",
        ".sh": "bash",
        ".bash": "bash",
        ".zsh": "bash",
        ".ps1": "powershell",
        ".yaml": "yaml",
        ".yml": "yaml",
        ".json": "json",
        ".toml": "toml",
        ".md": "markdown",
    }
    return mapping.get(ext, "")


class ContentCache:
    def __init__(self, token: Optional[str]):
        self.token = token
        self._cache: Dict[Tuple[str, str, str], Optional[List[str]]] = {}

    def fetch_lines(self, owner: str, repo: str, path: str, ref: str) -> Optional[List[str]]:
        key = (owner, repo, path + "@" + ref)
        if key in self._cache:
            return self._cache[key]
        url = f"{GITHUB_API}/repos/{owner}/{repo}/contents/{path}?ref={ref}"
        try:
            status, _headers, data = http_get(url, self.token)
            if status != 200:
                self._cache[key] = None
                return None
            payload = json.loads(data.decode("utf-8"))
            if isinstance(payload, dict) and payload.get("encoding") == "base64" and payload.get("content"):
                raw = base64.b64decode(payload["content"])
                text = raw.decode("utf-8", errors="replace")
                lines = text.splitlines()
                self._cache[key] = lines
                return lines
            # Fallback: attempt raw content via the download_url
            dl = payload.get("download_url") if isinstance(payload, dict) else None
            if dl:
                status2, _h2, raw2 = http_get(dl, self.token, accept="*/*")
                if status2 == 200:
                    text = raw2.decode("utf-8", errors="replace")
                    lines = text.splitlines()
                    self._cache[key] = lines
                    return lines
        except Exception:
            pass
        self._cache[key] = None
        return None


def safe_md(s: str) -> str:
    # Avoid triple-backtick collisions
    return s.replace("```", "``\u200b`")


def build_thread_markdown(
    owner: str,
    repo: str,
    thread: Dict,
    anchor: Optional[Anchor],
    content_cache: ContentCache,
    context_lines: int,
) -> str:
    t_id = thread.get("id")
    comments = thread.get("comments") or []
    header = [f"### Thread {t_id} (unresolved)"]

    code_block: List[str] = []
    if anchor:
        lines = content_cache.fetch_lines(owner, repo, anchor.path, anchor.commit_sha)
        if lines is not None:
            disp_start = max(1, anchor.start_line - context_lines)
            disp_end = min(len(lines), anchor.end_line + context_lines)
            lang = get_language_from_path(anchor.path)

            code_block.append(
                f"File `{anchor.path}` at `{anchor.commit_sha[:12]}` ({anchor.side})"
            )
            code_block.append(
                f"Marked lines: {anchor.start_line}-{anchor.end_line}; Displayed: {disp_start}-{disp_end} (context {context_lines})"
            )
            code_block.append("Legend: '>>' = marked lines; numbers are file line numbers")
            code_block.append("")
            code_block.append(f"```{lang}")
            width = len(str(disp_end))
            for idx in range(disp_start, disp_end + 1):
                mark = ">>" if anchor.start_line <= idx <= anchor.end_line else "  "
                content = lines[idx - 1]
                code_block.append(f"{mark} {idx:>{width}} | {safe_md(content)}")
            code_block.append("```")
        else:
            # Fallback to diff hunk if content not available
            primary = comments[0] if comments else {}
            diff = primary.get("diff_hunk") or thread.get("diff_hunk") or "(diff not available)"
            code_block.append("Code content not available at referenced commit; showing diff hunk:")
            code_block.append("")
            code_block.append("```diff")
            code_block.append(diff)
            code_block.append("```")
    else:
        code_block.append("No code anchor found for this thread.")

    convo: List[str] = []
    convo.append("Messages:")
    for c in comments:
        c_id = c.get("databaseId") or c.get("id")
        user = ((c.get("author") or {}).get("login")) or ((c.get("user") or {}).get("login")) or "unknown"
        created = c.get("createdAt") or c.get("created_at") or ""
        body = c.get("body") or ""
        convo.append(f"- {user} at {created} (comment id {c_id}):")
        if body.strip():
            convo.append("")
            convo.append(safe_md(body.strip()))
            convo.append("")

    # IDs section so users can reply via API
    ids: List[str] = []
    ids.append("IDs:")
    ids.append(f"- Thread ID: `{t_id}`")
    if comments:
        ids.append("- Comment IDs: " + ", ".join(f"`{(c.get('databaseId') or c.get('id'))}`" for c in comments))

    parts = []
    parts.extend(header)
    parts.append("")
    parts.extend(code_block)
    parts.append("")
    parts.extend(convo)
    parts.append("")
    parts.extend(ids)
    parts.append("")
    return "\n".join(parts)


def get_token(preferred: Optional[str]) -> Optional[str]:
    # Priority: explicit arg > env (GITHUB_TOKEN/GH_TOKEN) > gh auth token
    if preferred:
        return preferred.strip()
    env_tok = os.getenv("GITHUB_TOKEN") or os.getenv("GH_TOKEN")
    if env_tok:
        return env_tok.strip()
    gh = shutil.which("gh")
    if gh:
        try:
            out = subprocess.check_output([gh, "auth", "token"], stderr=subprocess.STDOUT, text=True)
            tok = out.strip()
            if tok:
                return tok
        except Exception:
            pass
    return None


def main() -> None:
    args = parse_args()
    token = get_token(args.token)

    if args.pr:
        pr_owner, pr_repo, pr_number = parse_pr_url(args.pr)
    else:
        if args.owner and args.repo and args.pull:
            pr_owner, pr_repo, pr_number = args.owner, args.repo, int(args.pull)
        else:
            inferred = infer_pr(token)
            if not inferred:
                print(
                    "No PR specified. Provide --pr or --owner/--repo/--pull, or run inside a PR checkout (gh pr view).",
                    file=sys.stderr,
                )
                sys.exit(2)
            pr_owner, pr_repo, pr_number = inferred

    if not token:
        print("Error: No GitHub token available. Set --token, GITHUB_TOKEN, GH_TOKEN, or authenticate with GitHub CLI (gh auth login).", file=sys.stderr)
        sys.exit(2)

    unresolved = list_unresolved_threads_graphql(pr_owner, pr_repo, pr_number, token)

    content_cache = ContentCache(token)

    md_lines: List[str] = []
    md_lines.append(f"## Unresolved Review Threads for {pr_owner}/{pr_repo} PR #{pr_number}")
    md_lines.append("")
    md_lines.append(f"Total unresolved threads: {len(unresolved)}")
    md_lines.append("")

    for t in unresolved:
        anchor = choose_anchor_from_comments(t.get("comments") or [])
        section = build_thread_markdown(
            pr_owner,
            pr_repo,
            t,
            anchor,
            content_cache,
            args.context,
        )
        md_lines.append(section)
        md_lines.append("")
        md_lines.append("---")
        md_lines.append("")

    sys.stdout.write("\n".join(md_lines).rstrip() + "\n")


if __name__ == "__main__":
    main()
