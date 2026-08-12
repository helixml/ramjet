// Package usage extracts token accounting and stream shape from OpenAI- and
// Anthropic-style response bodies (buffered or SSE).
package usage

import (
	"bytes"
	"encoding/json"
	"fmt"
)

type Accumulator struct {
	Prompt, Cached, Completion *float64
	FinishReason               string
	ContentChars               int
	ReasoningChars             int
	ToolCallDeltas             int
	Generated                  bool
}

func (a *Accumulator) FeedJSON(data []byte) {
	var object map[string]any
	if json.Unmarshal(data, &object) == nil {
		a.FeedObject(object)
	}
}

func (a *Accumulator) FeedSSELine(line []byte) {
	line = bytes.TrimSpace(line)
	if !bytes.HasPrefix(line, []byte("data:")) {
		return
	}
	payload := bytes.TrimSpace(line[len("data:"):])
	if len(payload) == 0 || bytes.Equal(payload, []byte("[DONE]")) {
		return
	}
	a.FeedJSON(payload)
}

func (a *Accumulator) FeedObject(object map[string]any) {
	if choices, ok := object["choices"].([]any); ok && len(choices) > 0 {
		if first, ok := choices[0].(map[string]any); ok {
			if reason, ok := first["finish_reason"].(string); ok {
				a.FinishReason = reason
			}
			for _, rawChoice := range choices {
				choice, ok := rawChoice.(map[string]any)
				if !ok {
					continue
				}
				node, _ := choice["delta"].(map[string]any)
				if len(node) == 0 {
					node, _ = choice["message"].(map[string]any)
				}
				if content, ok := node["content"].(string); ok {
					a.ContentChars += len([]rune(content))
					a.Generated = a.Generated || content != ""
				}
				if reasoning, ok := node["reasoning_content"].(string); ok {
					a.ReasoningChars += len([]rune(reasoning))
					a.Generated = a.Generated || reasoning != ""
				}
				if reasoning, ok := node["reasoning"].(string); ok {
					a.ReasoningChars += len([]rune(reasoning))
					a.Generated = a.Generated || reasoning != ""
				}
				if truthy(node["tool_calls"]) {
					a.ToolCallDeltas++
					a.Generated = true
				}
			}
		}
	}
	if reason, exists := object["stop_reason"]; exists && reason != nil {
		a.FinishReason = fmt.Sprint(reason)
	}
	if status, ok := object["status"].(string); ok && (status == "completed" || status == "incomplete" || status == "cancelled") {
		a.FinishReason = status
	}
	// Anthropic streams generated text/reasoning in a top-level delta object;
	// the Responses API uses a top-level delta string for output-text events.
	if delta, ok := object["delta"].(map[string]any); ok {
		for _, key := range []string{"text", "thinking", "reasoning", "partial_json"} {
			if value, ok := delta[key].(string); ok && value != "" {
				a.Generated = true
			}
		}
	} else if delta, ok := object["delta"].(string); ok && delta != "" {
		a.Generated = true
	}

	usage, _ := object["usage"].(map[string]any)
	if message, ok := object["message"].(map[string]any); ok {
		if messageUsage, ok := message["usage"].(map[string]any); ok && len(messageUsage) > 0 {
			usage = messageUsage
		}
	}
	if usage == nil {
		return
	}
	if value, ok := numberValue(usage["prompt_tokens"]); ok {
		a.Prompt = &value
		cachedFound := false
		if details, ok := usage["prompt_tokens_details"].(map[string]any); ok {
			if cached, ok := numberValue(details["cached_tokens"]); ok {
				a.Cached = &cached
				cachedFound = true
			}
		}
		if !cachedFound {
			if cached, ok := numberValue(usage["cached_tokens"]); ok {
				a.Cached = &cached
			}
		}
	}
	if value, ok := numberValue(usage["completion_tokens"]); ok {
		a.Completion = &value
	}
	if value, ok := numberValue(usage["input_tokens"]); ok {
		a.Prompt = &value
	}
	if value, ok := numberValue(usage["cache_read_input_tokens"]); ok {
		a.Cached = &value
	}
	if value, ok := numberValue(usage["output_tokens"]); ok {
		a.Completion = &value
	}
}

func FeedSSEChunk(accumulator *Accumulator, tail, chunk []byte) []byte {
	tail = append(tail, chunk...)
	for {
		idx := bytes.IndexByte(tail, '\n')
		if idx < 0 {
			return tail
		}
		accumulator.FeedSSELine(tail[:idx])
		tail = tail[idx+1:]
	}
}

func numberValue(value any) (float64, bool) {
	switch value := value.(type) {
	case float64:
		return value, true
	case json.Number:
		result, err := value.Float64()
		return result, err == nil
	default:
		return 0, false
	}
}

func truthy(value any) bool {
	switch value := value.(type) {
	case nil:
		return false
	case bool:
		return value
	case string:
		return value != ""
	case []any:
		return len(value) > 0
	case map[string]any:
		return len(value) > 0
	default:
		return true
	}
}
