import json
import unittest

from gemini_usage import complete_usage


def final(usage=None):
    return {"candidates": [{"index": 0, "finishReason": "STOP"}],
            "usageMetadata": usage if usage is not None else {
                "promptTokenCount": 10, "candidatesTokenCount": 3,
                "thoughtsTokenCount": 7, "totalTokenCount": 20}}


def sse(*frames):
    return b"".join(b"data: " + json.dumps(frame).encode() + b"\n\n" for frame in frames)


class UsageTests(unittest.TestCase):
    def test_thinking_is_charged_even_without_a_separate_thought_count(self):
        response = final()
        self.assertEqual(complete_usage(json.dumps(response)), (10, 10))
        del response["usageMetadata"]["thoughtsTokenCount"]
        self.assertEqual(complete_usage(sse(response), True), (10, 10))

    def test_prompt_growth_cannot_hide_decreasing_billed_output(self):
        earlier = final({"promptTokenCount": 10, "candidatesTokenCount": 1, "totalTokenCount": 110})
        later = final({"promptTokenCount": 100, "candidatesTokenCount": 1, "totalTokenCount": 110})
        self.assertIsNone(complete_usage(sse(earlier, later), True))

    def test_unknown_incomplete_and_inconsistent_metadata_cannot_release_funds(self):
        for response in [{}, {"usageMetadata": final()["usageMetadata"]},
                         final({"promptTokenCount": 10, "candidatesTokenCount": 3}),
                         final({"promptTokenCount": True, "candidatesTokenCount": 3, "totalTokenCount": 20}),
                         final(final()["usageMetadata"] | {"thoughtsTokenCount": 0}),
                         final(final()["usageMetadata"] | {"toolUsePromptTokenCount": 1}),
                         final(final()["usageMetadata"] | {"serviceTier": "PRIORITY"})]:
            self.assertIsNone(complete_usage(json.dumps(response)))
        self.assertIsNone(complete_usage(sse(final())[:-1], True))
        self.assertIsNone(complete_usage(sse(final(), {"error": "failed"}), True))
        self.assertIsNone(complete_usage(sse(final(), {}), True))
        self.assertIsNone(complete_usage(sse(final(), final({"promptTokenCount": 10,
                                                           "candidatesTokenCount": 2, "totalTokenCount": 12})), True))
        self.assertIsNone(complete_usage(b'{"usageMetadata":{},"usageMetadata":{}}'))


if __name__ == "__main__":
    unittest.main()
