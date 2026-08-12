// Package shims normalizes client requests for strict OpenAI-compatible
// engines and rewrites advertised model metadata. These are the accumulated
// Zed/Helix harness compatibility fixes — see DESIGN.md "Shims".
package shims

import (
	"bytes"
	"encoding/json"
	"fmt"
	"log"
	"strings"
)

func EndpointLabel(path string) string {
	switch {
	case strings.HasPrefix(path, "/v1/chat/completions"):
		return "chat"
	case strings.HasPrefix(path, "/v1/messages"):
		return "messages"
	case strings.HasPrefix(path, "/v1/responses"):
		return "responses"
	case strings.HasPrefix(path, "/v1/completions"):
		return "completions"
	default:
		return "other"
	}
}

func FinishReasonLabel(reason string) string {
	switch reason {
	case "stop", "end_turn", "completed":
		return "stop"
	case "length", "max_tokens", "incomplete":
		return "length"
	case "tool_calls", "tool_use":
		return "tool_calls"
	case "cancelled", "canceled":
		return "cancelled"
	default:
		return "other"
	}
}

func SanitizeRequestBody(endpoint string, body []byte, threshold int64) []byte {
	if (endpoint != "chat" && endpoint != "completions") || len(body) == 0 {
		return body
	}
	decoder := json.NewDecoder(bytes.NewReader(body))
	decoder.UseNumber()
	var object map[string]any
	if decoder.Decode(&object) != nil || object == nil {
		return body
	}

	var notes []string
	for _, field := range []string{"max_tokens", "max_completion_tokens"} {
		if number, ok := object[field].(json.Number); ok {
			if value, err := number.Int64(); err == nil && value >= threshold {
				delete(object, field)
				notes = append(notes, fmt.Sprintf("stripped %s>=%d", field, threshold))
			}
		}
	}
	if flattenContentParts(object) {
		notes = append(notes, "flattened content-parts arrays")
	}
	if effort, exists := object["reasoning_effort"]; exists {
		value, validString := effort.(string)
		valid := validString && (value == "none" || value == "minimal" || value == "low" || value == "medium" || value == "high" || value == "xhigh" || value == "max")
		if !valid {
			delete(object, "reasoning_effort")
			notes = append(notes, fmt.Sprintf("dropped reasoning_effort=%v", effort))
		}
	}
	if len(notes) == 0 {
		return body
	}
	result, err := json.Marshal(object)
	if err != nil {
		return body
	}
	log.Printf("[sanitize] %s: %s", endpoint, strings.Join(notes, ", "))
	return result
}

func flattenContentParts(object map[string]any) bool {
	messages, ok := object["messages"].([]any)
	if !ok {
		return false
	}
	changed := false
	for _, rawMessage := range messages {
		message, ok := rawMessage.(map[string]any)
		if !ok {
			continue
		}
		parts, ok := message["content"].([]any)
		if !ok {
			continue
		}
		var text strings.Builder
		for _, rawPart := range parts {
			switch part := rawPart.(type) {
			case string:
				text.WriteString(part)
			case map[string]any:
				if value, ok := part["text"].(string); ok {
					text.WriteString(value)
				}
			}
		}
		message["content"] = text.String()
		changed = true
	}
	return changed
}

func ShrinkAdvertisedContext(body []byte, margin int64) []byte {
	if margin <= 0 {
		return body
	}
	decoder := json.NewDecoder(bytes.NewReader(body))
	decoder.UseNumber()
	var object map[string]any
	if decoder.Decode(&object) != nil || object == nil {
		return body
	}
	data, ok := object["data"].([]any)
	if !ok {
		return body
	}
	changed := false
	for _, rawItem := range data {
		if item, ok := rawItem.(map[string]any); ok {
			changed = shrinkContextMap(item, margin) || changed
		}
	}
	if !changed {
		return body
	}
	result, err := json.Marshal(object)
	if err != nil {
		return body
	}
	return result
}

func shrinkContextMap(object map[string]any, margin int64) bool {
	changed := false
	for _, key := range []string{"max_model_len", "context_length", "max_context_length"} {
		number, ok := object[key].(json.Number)
		if !ok {
			continue
		}
		value, err := number.Int64()
		if err == nil && value > margin {
			object[key] = value - margin
			changed = true
		}
	}
	for _, rawChild := range object {
		if child, ok := rawChild.(map[string]any); ok {
			changed = shrinkContextMap(child, margin) || changed
		}
	}
	return changed
}
