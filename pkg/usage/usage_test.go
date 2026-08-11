package usage

import "testing"

func TestOpenAIUsage(t *testing.T) {
	a := &Accumulator{}
	a.FeedJSON([]byte(`{"choices":[{"message":{"content":"hi","reasoning_content":"thinking"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":90}}}`))
	if a.Prompt == nil || *a.Prompt != 100 || a.Cached == nil || *a.Cached != 90 || a.Completion == nil || *a.Completion != 5 {
		t.Fatalf("usage not extracted: %+v", a)
	}
	if a.FinishReason != "stop" || a.ContentChars != 2 || a.ReasoningChars != 8 {
		t.Fatalf("stream shape wrong: %+v", a)
	}
}

func TestAnthropicUsage(t *testing.T) {
	a := &Accumulator{}
	a.FeedJSON([]byte(`{"message":{"usage":{"input_tokens":50,"output_tokens":7,"cache_read_input_tokens":40}}}`))
	if a.Prompt == nil || *a.Prompt != 50 || a.Cached == nil || *a.Cached != 40 || a.Completion == nil || *a.Completion != 7 {
		t.Fatalf("anthropic usage not extracted: %+v", a)
	}
}

func TestSSEChunkReassembly(t *testing.T) {
	a := &Accumulator{}
	var tail []byte
	// usage split across two chunks mid-line
	tail = FeedSSEChunk(a, tail, []byte("data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\ndata: {\"usage\":{\"prompt_t"))
	tail = FeedSSEChunk(a, tail, []byte("okens\":10,\"completion_tokens\":3}}\n"))
	if len(tail) != 0 {
		t.Fatalf("tail should be drained, got %q", tail)
	}
	if a.Prompt == nil || *a.Prompt != 10 || a.Completion == nil || *a.Completion != 3 {
		t.Fatalf("split SSE usage not extracted: %+v", a)
	}
	if a.ContentChars != 1 {
		t.Fatalf("delta content not counted: %+v", a)
	}
}

func TestDoneAndGarbageIgnored(t *testing.T) {
	a := &Accumulator{}
	a.FeedSSELine([]byte("data: [DONE]"))
	a.FeedSSELine([]byte("data: {not json"))
	a.FeedSSELine([]byte(": comment"))
	if a.Prompt != nil || a.Completion != nil {
		t.Fatalf("garbage should not populate usage: %+v", a)
	}
}
