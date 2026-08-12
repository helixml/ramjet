package shims

import (
	"encoding/json"
	"strings"
	"testing"
)

func decode(t *testing.T, body []byte) map[string]any {
	t.Helper()
	var object map[string]any
	if err := json.Unmarshal(body, &object); err != nil {
		t.Fatal(err)
	}
	return object
}

func TestSanitizeStripsOversizedMaxTokens(t *testing.T) {
	body := []byte(`{"messages":[{"role":"user","content":"hi"}],"max_tokens":131072,"max_completion_tokens":262144}`)
	out := decode(t, SanitizeRequestBody("chat", body, 100000))
	if _, exists := out["max_tokens"]; exists {
		t.Fatal("max_tokens should be stripped")
	}
	if _, exists := out["max_completion_tokens"]; exists {
		t.Fatal("max_completion_tokens should be stripped")
	}
}

func TestSanitizeKeepsReasonableMaxTokens(t *testing.T) {
	body := []byte(`{"messages":[{"role":"user","content":"hi"}],"max_tokens":4096}`)
	out := SanitizeRequestBody("chat", body, 100000)
	if !strings.Contains(string(out), "4096") {
		t.Fatal("small max_tokens must survive")
	}
}

func TestSanitizeDropsOnlyInvalidReasoningEffort(t *testing.T) {
	for _, effort := range []string{"none", "minimal", "low", "medium", "high", "xhigh", "max"} {
		body := []byte(`{"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"` + effort + `"}`)
		out := decode(t, SanitizeRequestBody("chat", body, 100000))
		if out["reasoning_effort"] != effort {
			t.Fatalf("reasoning_effort=%s must survive", effort)
		}
	}
	body := []byte(`{"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"invalid"}`)
	out := decode(t, SanitizeRequestBody("chat", body, 100000))
	if _, exists := out["reasoning_effort"]; exists {
		t.Fatal("unsupported reasoning_effort should be dropped")
	}
}

func TestFlattenContentPartsIncludingEmptyPart(t *testing.T) {
	body := []byte(`{"messages":[{"role":"assistant","content":[{"type":"text","text":"Let me explore."},{"type":"text"}]},{"role":"user","content":"go on"}]}`)
	out := decode(t, SanitizeRequestBody("chat", body, 100000))
	messages := out["messages"].([]any)
	first := messages[0].(map[string]any)
	if first["content"] != "Let me explore." {
		t.Fatalf("parts should flatten to string, got %v", first["content"])
	}
	second := messages[1].(map[string]any)
	if second["content"] != "go on" {
		t.Fatal("string content must be untouched")
	}
}

func TestSanitizeOnlyChatAndCompletions(t *testing.T) {
	body := []byte(`{"max_tokens":999999}`)
	if string(SanitizeRequestBody("other", body, 100000)) != string(body) {
		t.Fatal("non-chat endpoints must pass through untouched")
	}
}

func TestShrinkAdvertisedContext(t *testing.T) {
	body := []byte(`{"object":"list","data":[{"id":"m","max_model_len":393216,"top_provider":{"context_length":393216}}]}`)
	out := decode(t, ShrinkAdvertisedContext(body, 131072))
	item := out["data"].([]any)[0].(map[string]any)
	if item["max_model_len"].(float64) != 262144 {
		t.Fatalf("max_model_len: want 262144, got %v", item["max_model_len"])
	}
	nested := item["top_provider"].(map[string]any)
	if nested["context_length"].(float64) != 262144 {
		t.Fatalf("nested context_length: want 262144, got %v", nested["context_length"])
	}
}

func TestShrinkDisabled(t *testing.T) {
	body := []byte(`{"data":[{"max_model_len":393216}]}`)
	if string(ShrinkAdvertisedContext(body, 0)) != string(body) {
		t.Fatal("margin 0 must be a no-op")
	}
}

func TestEndpointLabel(t *testing.T) {
	cases := map[string]string{
		"/v1/chat/completions": "chat",
		"/v1/messages":         "messages",
		"/v1/responses":        "responses",
		"/v1/completions":      "completions",
		"/v1/models":           "other",
	}
	for path, want := range cases {
		if got := EndpointLabel(path); got != want {
			t.Fatalf("%s: want %s got %s", path, want, got)
		}
	}
}
