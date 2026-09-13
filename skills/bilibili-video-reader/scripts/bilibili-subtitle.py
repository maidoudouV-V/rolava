#!/usr/bin/env python3
"""Read Bilibili subtitles through yt-dlp without exposing login cookies."""

from __future__ import annotations

import html
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Callable, Mapping, Sequence
from urllib.parse import parse_qs, urlsplit, urlunsplit
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

MAX_SUBTITLE_CHARS = 12_000
SUBTITLE_EDGE_CHARS = 6_000
YTDLP_TIMEOUT_SECONDS = 25
MAX_PROCESS_OUTPUT_CHARS = 1_000_000
BVID_PATTERN = re.compile(r"^BV[0-9A-Za-z]{10}$")
SUBTITLE_EXTENSIONS = {"srt", "vtt", "json", "ass", "txt"}


class SkillError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


def parse_args(arguments: Sequence[str]) -> dict[str, str]:
    allowed = {"url", "part"}
    values: dict[str, str] = {}
    for argument in arguments:
        if not argument.startswith("--") or "=" not in argument:
            raise SkillError("INVALID_INPUT", "参数必须使用 --name=value 格式")
        name, value = argument[2:].split("=", 1)
        if name not in allowed or not value:
            raise SkillError("INVALID_INPUT", f"不支持或缺少参数值：--{name}")
        values[name] = value
    if "url" not in values:
        raise SkillError("INVALID_INPUT", "缺少 --url")
    return values


def parse_part(value: object | None) -> int | None:
    if value is None or value == "":
        return None
    text = str(value)
    if not re.fullmatch(r"[1-9]\d{0,3}", text):
        raise SkillError("INVALID_INPUT", "part 必须是 1-9999 的整数")
    return int(text)


def normalize_video_input(raw_value: object, explicit_part: object | None = None) -> dict[str, object]:
    raw = str(raw_value or "").strip()
    if not raw or len(raw) > 2048:
        raise SkillError("INVALID_INPUT", "B站视频链接或 BV 号无效")

    part = parse_part(explicit_part)
    if BVID_PATTERN.fullmatch(raw):
        return {
            "url": f"https://www.bilibili.com/video/{raw}",
            "bvid": raw,
            "part": part or 1,
        }

    try:
        parsed = urlsplit(raw)
        port = parsed.port
    except ValueError as error:
        raise SkillError("INVALID_INPUT", "B站视频链接或 BV 号无效") from error
    if (
        parsed.scheme not in {"http", "https"}
        or not parsed.hostname
        or parsed.username
        or parsed.password
        or port is not None
    ):
        raise SkillError("INVALID_INPUT", "仅支持标准的B站视频链接")

    hostname = parsed.hostname.lower()
    is_bilibili = hostname == "bilibili.com" or hostname.endswith(".bilibili.com")
    is_short_link = hostname == "b23.tv" or hostname.endswith(".b23.tv")
    if not is_bilibili and not is_short_link:
        raise SkillError("INVALID_INPUT", "仅支持 bilibili.com、b23.tv 或 BV 号")

    bvid: str | None = None
    if is_bilibili:
        match = re.search(r"/video/(BV[0-9A-Za-z]{10})(?:/|$)", parsed.path)
        if not match:
            raise SkillError("INVALID_INPUT", "仅支持B站普通视频链接")
        bvid = match.group(1)
        if part is None:
            query_part = parse_qs(parsed.query).get("p")
            if query_part:
                part = parse_part(query_part[0])
        normalized_url = f"https://www.bilibili.com/video/{bvid}"
    else:
        if not parsed.path or parsed.path == "/":
            raise SkillError("INVALID_INPUT", "b23.tv 短链接无效")
        normalized_url = urlunsplit(("https", parsed.netloc, parsed.path, parsed.query, ""))

    return {"url": normalized_url, "bvid": bvid, "part": part or 1}


def resolve_short_link(video: Mapping[str, object], explicit_part: object | None) -> dict[str, object]:
    request = Request(str(video["url"]), headers={"User-Agent": "Mozilla/5.0"})
    try:
        with urlopen(request, timeout=3) as response:
            final_url = response.geturl()
    except HTTPError as error:
        # 重定向目标即使拒绝页面请求，仍可从最终 URL 提取视频和分P。
        final_url = error.geturl()
        error.close()
    except (URLError, OSError) as error:
        raise SkillError("UPSTREAM_ERROR", "无法解析 b23.tv 短链接") from error
    resolved = normalize_video_input(final_url, explicit_part)
    if not resolved["bvid"]:
        raise SkillError("INVALID_INPUT", "b23.tv 短链接未指向普通视频")
    return resolved


def cookie_header_to_netscape(raw_value: object | None) -> str:
    raw = raw_value.strip() if isinstance(raw_value, str) else ""
    if not raw:
        raise SkillError("MISSING_COOKIE", "缺少 BILIBILI_COOKIE 环境变量")
    if re.search(r"[\r\n\t]", raw):
        raise SkillError("LOGIN_REQUIRED", "BILIBILI_COOKIE 格式无效，请更新登录 Cookie")

    cookies: dict[str, str] = {}
    for item in raw.split(";"):
        pair = item.strip()
        if not pair:
            continue
        if "=" not in pair:
            raise SkillError("LOGIN_REQUIRED", "BILIBILI_COOKIE 格式无效，请更新登录 Cookie")
        name, value = pair.split("=", 1)
        name = name.strip()
        value = value.strip()
        if not re.fullmatch(r"[!#$%&'*+.^_`|~0-9A-Za-z-]+", name):
            raise SkillError("LOGIN_REQUIRED", "BILIBILI_COOKIE 格式无效，请更新登录 Cookie")
        cookies[name] = value

    if not cookies.get("SESSDATA"):
        raise SkillError("LOGIN_REQUIRED", "BILIBILI_COOKIE 不包含有效的B站登录状态")

    lines = ["# Netscape HTTP Cookie File"]
    lines.extend(
        f".bilibili.com\tTRUE\t/\tTRUE\t0\t{name}\t{value}"
        for name, value in cookies.items()
    )
    return "\n".join(lines) + "\n"


def build_ytdlp_args(video: Mapping[str, object], cookie_path: Path, output_directory: Path) -> list[str]:
    return [
        "--skip-download",
        "--write-subs",
        "--write-auto-subs",
        "--sub-langs",
        "all,-danmaku",
        "--sub-format",
        "srt/best",
        "--write-info-json",
        "--no-progress",
        "--no-playlist",
        "--cookies",
        str(cookie_path),
        "--output",
        str(output_directory / "video.%(ext)s"),
        f"{video['url']}?p={video['part']}",
    ]


def _decoded_process_text(value: object | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")[:MAX_PROCESS_OUTPUT_CHARS]
    return str(value)[:MAX_PROCESS_OUTPUT_CHARS]


def run_ytdlp(
    *,
    args: Sequence[str],
    cwd: Path,
    env: Mapping[str, str],
    timeout_seconds: int = YTDLP_TIMEOUT_SECONDS,
) -> dict[str, object]:
    command = [sys.executable, "-m", "yt_dlp", *args]
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=dict(env),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        return {
            "returncode": None,
            "stdout": _decoded_process_text(error.stdout),
            "stderr": _decoded_process_text(error.stderr),
            "timed_out": True,
        }
    except OSError as error:
        raise SkillError("UPSTREAM_ERROR", "无法启动 yt-dlp") from error
    return {
        "returncode": result.returncode,
        "stdout": result.stdout[:MAX_PROCESS_OUTPUT_CHARS],
        "stderr": result.stderr[:MAX_PROCESS_OUTPUT_CHARS],
        "timed_out": False,
    }


def looks_like_login_error(output: object) -> bool:
    return bool(re.search(r"login|logged in|cookie|account|登录|账号", str(output or ""), re.I))


def map_ytdlp_failure(result: Mapping[str, object]) -> None:
    if result.get("timed_out"):
        raise SkillError("TIMEOUT", "读取B站字幕超时")
    output = f"{result.get('stdout', '')}\n{result.get('stderr', '')}"
    if looks_like_login_error(output):
        raise SkillError("LOGIN_REQUIRED", "BILIBILI_COOKIE 已失效或当前账号无权读取该视频字幕")
    if re.search(r"unsupported url|invalid url", output, re.I):
        raise SkillError("INVALID_INPUT", "B站视频链接无效或暂不支持")
    raise SkillError("UPSTREAM_ERROR", "读取B站字幕失败")


def clean_cue_text(value: object) -> str:
    text = str(value or "")
    text = re.sub(
        r"</?(?:b|i|u|s|font|c|v|lang|ruby|rt)(?:\.[\w-]+)*(?:\s+[^<>]*)?\s*/?>"
        r"|<(?:\d{2}:)?\d{2}:\d{2}\.\d{3}>",
        "",
        text,
        flags=re.I,
    )
    text = re.sub(r"\{\\[^}]+}", "", text)
    text = text.replace(r"\N", "\n")
    text = html.unescape(text)
    return " ".join(line.strip() for line in text.splitlines() if line.strip()).strip()


def timed_subtitle_to_text(raw_value: object) -> str:
    raw = str(raw_value or "").lstrip("\ufeff").replace("\r\n", "\n").replace("\r", "\n")
    cues: list[str] = []
    for block in re.split(r"\n{2,}", raw):
        lines = [line.strip() for line in block.splitlines()]
        if not lines or lines[0] == "WEBVTT" or lines[0].startswith("NOTE"):
            continue
        timestamp_index = next((index for index, line in enumerate(lines) if "-->" in line), -1)
        if timestamp_index < 0:
            continue
        text = clean_cue_text("\n".join(lines[timestamp_index + 1 :]))
        if text and (not cues or text != cues[-1]):
            cues.append(text)
    return " ".join(cues)


def ass_to_text(raw_value: object) -> str:
    cues: list[str] = []
    raw = str(raw_value or "").replace("\r\n", "\n").replace("\r", "\n")
    for line in raw.splitlines():
        if not line.startswith("Dialogue:"):
            continue
        fields = line[len("Dialogue:") :].split(",")
        if len(fields) < 10:
            continue
        text = clean_cue_text(",".join(fields[9:]))
        if text and (not cues or text != cues[-1]):
            cues.append(text)
    return " ".join(cues)


def json_subtitle_to_text(raw_value: object) -> str:
    try:
        data = json.loads(str(raw_value))
    except (TypeError, json.JSONDecodeError) as error:
        raise SkillError("UPSTREAM_ERROR", "字幕文件格式无效") from error
    body = data.get("body", []) if isinstance(data, dict) else []
    if not isinstance(body, list):
        body = []
    return " ".join(
        text
        for item in body
        if isinstance(item, dict)
        if (text := clean_cue_text(item.get("content", "")))
    )


def subtitle_to_text(extension: str, raw_value: object) -> str:
    if extension in {"srt", "vtt"}:
        return timed_subtitle_to_text(raw_value)
    if extension == "ass":
        return ass_to_text(raw_value)
    if extension == "json":
        return json_subtitle_to_text(raw_value)
    return str(raw_value or "").strip()


def detect_subtitle_language(file_name: str) -> str:
    match = re.fullmatch(r"video(?:\.([^.]+))?\.(?:srt|vtt|json|ass|txt)", file_name, re.I)
    return match.group(1) if match and match.group(1) else "unknown"


def language_priority(language: object) -> int:
    value = str(language or "").lower()
    if value in {"zh-hans", "zh-cn", "zh"}:
        return 0
    if value == "ai-zh" or value.startswith("ai-zh-"):
        return 1
    if value.startswith("zh"):
        return 2
    return 3


def truncate_subtitle(value: str) -> dict[str, object]:
    total_chars = len(value)
    if total_chars <= MAX_SUBTITLE_CHARS:
        return {"subtitle": value, "total_chars": total_chars, "truncated": False}
    return {
        "subtitle": (
            value[:SUBTITLE_EDGE_CHARS]
            + "\n...[中间字幕已省略]...\n"
            + value[-SUBTITLE_EDGE_CHARS:]
        ),
        "total_chars": total_chars,
        "truncated": True,
    }


def extract_bvid(info: Mapping[str, object], fallback: object | None) -> str | None:
    for candidate in (info.get("id"), info.get("webpage_url"), info.get("original_url"), fallback):
        match = re.search(r"BV[0-9A-Za-z]{10}", str(candidate or ""))
        if match:
            return match.group(0)
    return None


Runner = Callable[..., Mapping[str, object]]


def read_subtitle(
    options: Mapping[str, object],
    *,
    environment: Mapping[str, str] | None = None,
    runner: Runner | None = None,
) -> dict[str, object]:
    runtime_environment = os.environ if environment is None else environment
    execute_ytdlp = run_ytdlp if runner is None else runner
    video = normalize_video_input(options.get("url"), options.get("part"))
    cookie_content = cookie_header_to_netscape(runtime_environment.get("BILIBILI_COOKIE"))
    if not video["bvid"]:
        video = resolve_short_link(video, options.get("part"))

    with tempfile.TemporaryDirectory(prefix="bilibili-subtitle-") as temporary:
        temporary_directory = Path(temporary)
        cookie_path = temporary_directory / "cookies.txt"
        descriptor = os.open(cookie_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as cookie_file:
            cookie_file.write(cookie_content)

        child_environment = dict(runtime_environment)
        child_environment.pop("BILIBILI_COOKIE", None)
        args = build_ytdlp_args(video, cookie_path, temporary_directory)
        result = execute_ytdlp(args=args, cwd=temporary_directory, env=child_environment)
        if result.get("timed_out") or result.get("returncode") != 0:
            map_ytdlp_failure(result)

        files = list(temporary_directory.iterdir())
        subtitle_files = sorted(
            (
                file
                for file in files
                if file.is_file()
                and file.name.startswith("video.")
                and file.suffix.lower().lstrip(".") in SUBTITLE_EXTENSIONS
                and not file.name.endswith(".info.json")
            ),
            key=lambda file: (language_priority(detect_subtitle_language(file.name)), file.name),
        )
        if not subtitle_files:
            output = f"{result.get('stdout', '')}\n{result.get('stderr', '')}"
            if looks_like_login_error(output):
                raise SkillError(
                    "LOGIN_REQUIRED",
                    "BILIBILI_COOKIE 已失效或当前账号无权读取该视频字幕",
                )
            raise SkillError("NO_SUBTITLE", "该视频没有可读取的字幕")

        selected = subtitle_files[0]
        extension = selected.suffix.lower().lstrip(".")
        raw_subtitle = selected.read_text(encoding="utf-8-sig", errors="replace")
        text = subtitle_to_text(extension, raw_subtitle)
        if not text:
            raise SkillError("NO_SUBTITLE", "该视频字幕内容为空")

        info: dict[str, object] = {}
        info_file = next((file for file in files if file.name.endswith(".info.json")), None)
        if info_file:
            try:
                loaded = json.loads(info_file.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                raise SkillError("UPSTREAM_ERROR", "视频信息格式无效") from error
            if isinstance(loaded, dict):
                info = loaded

        shortened = truncate_subtitle(text)
        language = detect_subtitle_language(selected.name)
        return {
            "title": info.get("title") if isinstance(info.get("title"), str) else None,
            "bvid": extract_bvid(info, video.get("bvid")),
            "part": video["part"],
            "language": language,
            "source": "ai" if language.lower().startswith("ai-") else "manual",
            **shortened,
        }


def main() -> int:
    try:
        result = read_subtitle(parse_args(sys.argv[1:]))
        print(json.dumps(result, ensure_ascii=False, separators=(",", ":")))
        return 0
    except SkillError as error:
        print(
            json.dumps({"error": str(error), "code": error.code}, ensure_ascii=False, separators=(",", ":")),
            file=sys.stderr,
        )
        return 1
    except Exception:
        print(
            json.dumps({"error": "读取B站字幕失败", "code": "UPSTREAM_ERROR"}, ensure_ascii=False),
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
