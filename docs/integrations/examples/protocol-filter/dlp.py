#!/usr/bin/python3
"""Offline Anthropic Messages DLP example for Ego's bidirectional v1 filter.

Rules cover recognizable credentials and labeled secrets in JSON string
values and pasted developer files/logs. This is not a complete secret detector.
Saved originals are private local journal data, not an encrypted vault.
"""
import base64
import json
import re
import sys
import uuid

MAX_BYTES = 16 * 1024 * 1024
SECRET = re.compile(
    r"-----BEGIN (?P<label>(?:(?:RSA|EC|DSA|OPENSSH|ENCRYPTED) )?PRIVATE KEY)-----"
    r"[\s\S]*?-----END (?P=label)-----"
    r"|\bsk-[A-Za-z0-9_-]{16,}"
    r"|\b(?:gh[pousr]_|github_pat_)[A-Za-z0-9_.-]{16,}"
    r"|\b(?:glpat-|gloas-|gldt-|glrt-|glrtr-|glcbt-)[A-Za-z0-9_-]{16,}"
    r"|\b(?:npm_|pypi-)[A-Za-z0-9_-]{16,}"
    r"|\bxox[baprs]-[A-Za-z0-9-]{16,}"
    r"|\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}"
    r"|\bAIza[A-Za-z0-9_-]{20,}"
    r"|\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"
    r"|\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"
    r"|\b[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+\b"
)
# Keep delimiters, names and hosts readable. Values, rather than complete
# assignments, become journal entries so the same credential reuses one token.
FIELD = (r"(?:[A-Za-z][A-Za-z0-9]*_)*(?:password|passwd|pwd|secret|token|"
         r"api_?key|access_?key|secret_?access_?key|secret_?key|client_?secret|"
         r"private_?key|_?authToken|_auth|x-api-key|api-key)")
SENSITIVE_FIELD = re.compile(r"(?:" + FIELD + r"|(?:proxy-)?authorization)", re.I)
CONTEXT = [
    re.compile(r"\b[A-Za-z][A-Za-z0-9+.-]*://(?P<secret>[^/\s\"'<>@]+:[^/\s\"'<>@]*)@"),
    re.compile(r"\b(?:proxy-)?authorization[\"']?\s*[:=]\s*[\"']?(?:Bearer|Basic)\s+"
               r"(?P<secret>[A-Za-z0-9._~+/-]+=*)", re.I),
    re.compile(r"[?&](?:" + FIELD + r"|X-Amz-(?:Signature|Security-Token)|sig|signature)="
               r"(?P<secret>[^&\s\"'<>#]+)", re.I),
    re.compile(r"(?<![\w])" + FIELD + r"[\"']?\s*[:=]\s*(?:"
               r'"(?P<double>(?:\\.|[^"\\])*)"'
               r"|'(?P<single>[^']*)'"
               r"|(?P<bare>[^\s\"'`,;}&]+))", re.I),
]


def token(key):
    return "__EGO_DLP_" + key.removeprefix("dlp.") + "__"


def mapping(values):
    return {token(k): v for k, v in values.items()
            if re.fullmatch(r"dlp\.[0-9a-f]{32}", k)}


def walk(value, transform, field_transform=None):
    if isinstance(value, str):
        return transform(value)
    if isinstance(value, list):
        return [walk(v, transform, field_transform) for v in value]
    if isinstance(value, dict):
        if any(SECRET.search(k) for k in value):
            raise ValueError("sensitive JSON object key is unsupported")
        return {k: field_transform(v) if field_transform is not None
                and isinstance(v, str) and SENSITIVE_FIELD.fullmatch(k)
                else walk(v, transform, field_transform) for k, v in value.items()}
    return value


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()


def decode_body(frame):
    body = base64.b64decode(frame["body_base64"], validate=True)
    if len(body) > MAX_BYTES:
        raise ValueError("body limit")
    return body


def controls(body):
    # Refuse rather than silently change model settings, roles, block tags,
    # tool declarations or schemas when a captured value collides with them.
    def blocks(content):
        if not isinstance(content, list):
            return None
        return [({k: v for k, v in block.items()
                  if k not in {"text", "input", "content"}},
                 blocks(block.get("content"))) for block in content]

    return ({k: v for k, v in body.items() if k not in {"messages", "system", "metadata"}},
            blocks(body.get("system")),
            [({k: v for k, v in message.items() if k != "content"},
              blocks(message.get("content"))) for message in body.get("messages", [])])


def request(frame):
    originals = mapping(frame["state"]["values"])
    inverse = {original: placeholder for placeholder, original in originals.items()}
    writes = {}
    # A restored value can return in later history without its original label
    # (for example a password echoed in assistant prose). Mask it again even
    # when no recognizable credential prefix remains. Shield existing tokens.
    def replay(text):
        if not inverse:
            return text
        choices = list(inverse.values()) + sorted(inverse, key=len, reverse=True)
        return re.sub("|".join(re.escape(s) for s in choices),
                      lambda m: inverse.get(m.group(), m.group()), text)

    def save(original):
        if not original or original in originals or any(token(k) == original for k in writes):
            return original
        # An assignment may contain an already masked URL or token. Persist
        # its complete original, never a nested placeholder requiring recursion.
        known = {**originals, **{token(k): v for k, v in writes.items()}}
        original = re.sub(r"__EGO_DLP_[0-9a-f]{32}__",
                          lambda m: known.get(m.group(), m.group()), original)
        if original not in inverse:
            if len(original.encode()) > 4096:
                raise ValueError("secret exceeds session variable limit")
            key = "dlp." + uuid.uuid4().hex
            inverse[original] = token(key)
            writes[key] = original
        return inverse[original]

    def mask(text):
        text = replay(text)
        for rule in CONTEXT:
            def replace(match):
                group = next(g for g in ("secret", "double", "single", "bare")
                             if g in match.re.groupindex and match.group(g) is not None)
                start, end = match.span(group)
                return (match.string[match.start():start] + save(match.group(group))
                        + match.string[end:match.end()])
            text = rule.sub(replace, text)
        # A URL's masked userinfo can resemble an email. Never wrap a token in
        # another token: that would require recursive, order-dependent restore.
        text = SECRET.sub(lambda m: m.group() if "__EGO_DLP_" in m.group()
                          else save(m.group()), text)
        return replay(text)

    original = json.loads(decode_body(frame))
    masked = walk(original, mask, save)
    # Discovery can occur in a later field than an earlier bare occurrence.
    masked = walk(masked, replay)
    if controls(original) != controls(masked):
        raise ValueError("masking would change provider control fields")
    body = encode(masked)
    return {"version": 1, "body_base64": base64.b64encode(body).decode(),
            "identity_headers": [], "expected_revision": frame["state"]["revision"],
            "state_writes": writes}


class Response:
    def __init__(self, frame):
        self.originals = mapping(frame["state"]["values"])
        self.saved_state = frame["state"]
        self.buffer = b""
        self.pending = {}
        self.sequence = 0
        self.status = frame["status"]

    def restore(self, text, channel, argument=False, final=False):
        text = self.pending.pop(channel, "") + text
        out = []
        i = 0
        while i < len(text):
            remaining = text[i:]
            complete = next((t for t in self.originals if remaining.startswith(t)), None)
            if complete is not None:
                original = self.originals[complete]
                out.append(json.dumps(original, ensure_ascii=False)[1:-1] if argument else original)
                i += len(complete)
            elif not final and any(t.startswith(remaining) for t in self.originals):
                self.pending[channel] = remaining
                if len(self.pending) > 64:
                    raise ValueError("too many pending content blocks")
                break
            else:
                out.append(text[i])
                i += 1
        return "".join(out)

    def event(self, raw):
        # Buffer complete physical SSE events; token fragments are then carried
        # by content-block index, independently of HTTP chunk boundaries.
        lines = raw.replace(b"\r\n", b"\n").split(b"\n")
        data = [line[5:].lstrip(b" ") for line in lines if line.startswith(b"data:")]
        if not data:
            return raw + b"\n\n"
        value = json.loads(b"\n".join(data))
        kind = value.get("type")
        index = value.get("index", 0)
        prefix = b"".join(line + b"\n" for line in lines if not line.startswith(b"data:"))
        extra = b""
        if kind == "content_block_delta":
            delta = value["delta"]
            if delta.get("type") == "text_delta":
                delta["text"] = self.restore(delta["text"], (index, "text"))
            elif delta.get("type") == "input_json_delta":
                delta["partial_json"] = self.restore(delta["partial_json"], (index, "argument"), argument=True)
        elif kind == "content_block_start":
            value["content_block"] = walk(value["content_block"],
                lambda s: self.restore(s, (index, "initial"), final=True))
        elif kind == "content_block_stop":
            for channel, pending in list(self.pending.items()):
                if channel[0] != index:
                    continue
                del self.pending[channel]
                argument = channel[1] == "argument"
                delta = {"type": "input_json_delta" if argument else "text_delta",
                         "partial_json" if argument else "text": pending}
                extra += b"event: content_block_delta\ndata: " + encode(
                    {"type": "content_block_delta", "index": index, "delta": delta}) + b"\n\n"
        return extra + prefix + b"data: " + encode(value) + b"\n\n"

    def frame(self, frame):
        if frame["sequence"] != self.sequence or frame["state"] != self.saved_state:
            raise ValueError("response identity changed")
        self.sequence += 1
        self.buffer += decode_body(frame)
        if len(self.buffer) > MAX_BYTES:
            raise ValueError("response event buffer limit")
        out = bytearray()
        # Non-200 errors are bounded JSON rather than SSE.
        if self.status != 200:
            if frame["eof"]:
                value = walk(json.loads(self.buffer),
                             lambda s: self.restore(s, (0, "error"), final=True))
                out.extend(encode(value))
                self.buffer = b""
        else:
            while True:
                delimiter = re.search(br"\r?\n\r?\n", self.buffer)
                if delimiter is None:
                    break
                raw = self.buffer[:delimiter.start()]
                self.buffer = self.buffer[delimiter.end():]
                out.extend(self.event(raw))
            if frame["eof"]:
                if self.buffer.strip():
                    out.extend(self.event(self.buffer))
                self.buffer = b""
                if self.pending:
                    raise ValueError("unfinished response content block")
        if len(out) > MAX_BYTES:
            raise ValueError("response output limit")
        return {"version": 1, "sequence": frame["sequence"],
                "body_base64": base64.b64encode(out).decode()}


def main():
    line = sys.stdin.buffer.readline(22_435_161)
    frame = json.loads(line)
    if frame["version"] != 1 or frame["provider"] != "anthropic":
        raise ValueError("this example requires v1 Anthropic Messages")
    if frame["direction"] == "request":
        sys.stdout.buffer.write(encode(request(frame)))
        return
    if frame["direction"] != "response":
        raise ValueError("unknown direction")
    response = Response(frame)
    while True:
        sys.stdout.buffer.write(encode(response.frame(frame)) + b"\n")
        sys.stdout.buffer.flush()
        if frame["eof"]:
            if sys.stdin.buffer.read(1):
                raise ValueError("unexpected input after EOF")
            break
        line = sys.stdin.buffer.readline(22_435_161)
        frame = json.loads(line)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError, UnicodeError, OSError):
        # Do not print secrets or raw provider bytes on error.
        raise SystemExit(1) from None
