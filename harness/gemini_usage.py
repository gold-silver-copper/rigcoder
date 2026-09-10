"""Conservative settlement from complete trusted Gemini response bodies.

UsageMetadata totalTokenCount includes prompt, thoughts and candidates:
https://ai.google.dev/api/generate-content#UsageMetadata
Unknown/inconsistent data returns None, preserving the full reservation.
"""
import json

from gemini_budget import MAX_INPUT, MAX_OUTPUT


def complete_usage(body, stream=False):
    def unique(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("duplicate key")
            result[key] = value
        return result

    def decode(value):
        return json.loads(value, object_pairs_hook=unique,
                          parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite JSON")))

    try:
        if stream:
            text = body.decode("utf-8").replace("\r\n", "\n")
            if not text.endswith("\n\n") or text.count("\n\n") > 100_000:
                return None
            frames = []
            ended = False
            for block in text.split("\n\n"):
                data = []
                for line in block.split("\n"):
                    if not line or line.startswith(":"):
                        continue
                    if not line.startswith("data:"):
                        return None
                    data.append(line[5:].removeprefix(" "))
                if not data:
                    continue
                if ended:
                    return None
                payload = "\n".join(data)
                if payload == "[DONE]":
                    ended = True
                else:
                    frames.append(decode(payload))
        else:
            frames = [decode(body)]
        if not frames:
            return None
        finished = False
        previous = (0, 0, 0, 0)
        result = None
        for frame in frames:
            if not isinstance(frame, dict) or "error" in frame:
                return None
            candidates = frame.get("candidates", [])
            if not isinstance(candidates, list) or len(candidates) > 1:
                return None
            for candidate in candidates:
                if not isinstance(candidate, dict) or candidate.get("index", 0) != 0:
                    return None
                reason = candidate.get("finishReason")
                if reason is not None:
                    if reason not in ("STOP", "MAX_TOKENS"):
                        return None
                    finished = True
            usage = frame.get("usageMetadata")
            result = None
            if usage is None:
                continue
            if not isinstance(usage, dict) or usage.get("serviceTier") not in (None, "STANDARD"):
                return None
            prompt, candidates, total = (usage.get(key) for key in
                                        ("promptTokenCount", "candidatesTokenCount", "totalTokenCount"))
            if any(type(value) is not int or value < 0 for value in (prompt, candidates, total)):
                return None
            output = total - prompt
            if not (0 < prompt <= MAX_INPUT and 0 <= candidates <= output <= MAX_OUTPUT):
                return None
            thoughts = usage.get("thoughtsTokenCount")
            if thoughts is not None and (type(thoughts) is not int or thoughts != output - candidates):
                return None
            # Server-side tools are outside the authorized request subset.
            if usage.get("toolUsePromptTokenCount", 0) != 0:
                return None
            current = (prompt, candidates, total, output)
            if any(new < old for new, old in zip(current, previous)):
                return None
            previous = current
            result = (prompt, output)
        return result if finished else None
    except (ValueError, TypeError, UnicodeError, RecursionError):
        return None
