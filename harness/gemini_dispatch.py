"""Fixed-endpoint Gemini dispatcher for the trusted experiment host.

Not yet exposed to candidate processes. Every HTTP attempt reserves the model's
full input/output allowance. Responses do not release reservations yet, avoiding
claims about incomplete streaming usage. No retries or redirect following.
"""
import http.client
import json

from gemini_budget import MAX_INPUT, MAX_OUTPUT

HOST = "generativelanguage.googleapis.com"
MODEL = "gemini-3.8-flash"
TOP = {"contents", "systemInstruction", "tools", "toolConfig", "generationConfig", "safetySettings"}
CONFIG = {"candidateCount", "maxOutputTokens", "temperature", "topP", "topK", "stopSequences", "thinkingConfig", "responseMimeType", "responseSchema", "responseJsonSchema", "seed", "responseModalities"}


def validate(body):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result

    if not isinstance(body, bytes) or len(body) > 4_000_000:
        raise ValueError("invalid request body size")
    request = json.loads(body, object_pairs_hook=pairs,
                         parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite JSON")))
    if not isinstance(request, dict) or set(request) - TOP:
        raise ValueError("unsupported request fields")
    config = request.get("generationConfig") or {}
    if not isinstance(config, dict) or set(config) - CONFIG:
        raise ValueError("unsupported generation settings")
    if config.get("candidateCount") not in (None, 1) or isinstance(config.get("candidateCount"), bool):
        raise ValueError("exactly one candidate is permitted")
    output = config.get("maxOutputTokens")
    if output is not None and (type(output) is not int or not 1 <= output <= MAX_OUTPUT):
        raise ValueError("invalid output token bound")
    if config.get("responseModalities") not in (None, ["TEXT"]):
        raise ValueError("only text output is permitted")
    for tool in request.get("tools") or []:
        if (not isinstance(tool, dict) or "functionDeclarations" not in tool
                or set(tool) - {"functionDeclarations", "codeExecution"}
                or tool.get("codeExecution") is not None):
            raise ValueError("only local function declarations are permitted")
    contents = request.get("contents")
    if not isinstance(contents, list) or not contents:
        raise ValueError("contents must be a nonempty list")
    if request.get("systemInstruction") is not None:
        contents = contents + [request["systemInstruction"]]
    for content in contents:
        if not isinstance(content, dict) or set(content) - {"role", "parts"}:
            raise ValueError("unsupported content")
        parts = content.get("parts")
        if not isinstance(parts, list):
            raise ValueError("parts must be a list")
        for part in parts:
            if not isinstance(part, dict) or set(part) - {"text", "functionCall", "functionResponse", "thought", "thoughtSignature"}:
                raise ValueError("only text and local function content is permitted")
            if "functionResponse" in part:
                response = part["functionResponse"]
                # Gemini also accepts media parts in this envelope. Tool result
                # JSON remains arbitrary, but media inputs are outside this run.
                if not isinstance(response, dict) or set(response) - {"name", "id", "response"}:
                    raise ValueError("unsupported function response envelope")
    return body


def send(budget, phase, body, api_key, stream=False):
    """Return (HTTP status, content type, bytes); reserve before network access.

    The caller must keep the key and ledger inaccessible to candidate code and
    prevent all alternate egress. Failure leaves the reservation charged.
    """
    body = validate(body)
    budget.reserve(phase, MAX_INPUT, MAX_OUTPUT)
    action = "streamGenerateContent?alt=sse" if stream else "generateContent"
    connection = http.client.HTTPSConnection(HOST, timeout=30)
    try:
        connection.request("POST", f"/v1beta/models/{MODEL}:{action}", body=body,
                           headers={"Content-Type": "application/json", "x-goog-api-key": api_key})
        response = connection.getresponse()
        result = response.read(32_000_001)
        if len(result) > 32_000_000:
            raise ValueError("provider response exceeded capture limit")
        return response.status, response.getheader("Content-Type"), result
    finally:
        connection.close()
