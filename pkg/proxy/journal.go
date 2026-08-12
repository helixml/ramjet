package proxy

import (
	"encoding/json"
	"log"
	"sync/atomic"
	"time"

	"github.com/helixml/mini-dynamo/pkg/config"
	"github.com/helixml/mini-dynamo/pkg/router"
	"github.com/helixml/mini-dynamo/pkg/usage"
)

const routeJournalVersion = 3

type routeJournal struct {
	enabled  bool
	sequence atomic.Uint64
}

type routeStartRecord struct {
	Version           int                     `json:"v"`
	Event             string                  `json:"event"`
	Sequence          uint64                  `json:"seq"`
	UnixMS            int64                   `json:"unix_ms"`
	Endpoint          string                  `json:"endpoint"`
	RequestBytes      int                     `json:"request_bytes"`
	TotalBlocks       int                     `json:"total_blocks"`
	Chosen            int                     `json:"chosen"`
	Outcome           string                  `json:"outcome"`
	Rotation          int                     `json:"rotation"`
	Alpha             float64                 `json:"alpha"`
	MaxAffinityBlocks int                     `json:"max_affinity_blocks"`
	ChunkBytes        int                     `json:"chunk_bytes"`
	LoadUnitBytes     int                     `json:"load_unit_bytes"`
	MaxLoadUnits      int                     `json:"max_load_units"`
	ScoreTieBreak     string                  `json:"score_tie_break"`
	Candidates        []router.CandidateState `json:"candidates"`
}

type routeFinishRecord struct {
	Version          int      `json:"v"`
	Event            string   `json:"event"`
	Sequence         uint64   `json:"seq"`
	UnixMS           int64    `json:"unix_ms"`
	Result           string   `json:"result"`
	Upstream         int      `json:"upstream"`
	Status           int      `json:"status"`
	DurationMS       float64  `json:"duration_ms"`
	FirstByteMS      *float64 `json:"first_byte_ms,omitempty"`
	TTFTMS           *float64 `json:"ttft_ms,omitempty"`
	ResponseBytes    int64    `json:"response_bytes"`
	PromptTokens     *float64 `json:"prompt_tokens,omitempty"`
	CachedTokens     *float64 `json:"cached_tokens,omitempty"`
	CompletionTokens *float64 `json:"completion_tokens,omitempty"`
}

func (j *routeJournal) start(endpoint string, requestBytes int, decision router.Decision, cfg config.Config) uint64 {
	if j == nil || !j.enabled || endpoint == "other" {
		return 0
	}
	sequence := j.sequence.Add(1)
	record := makeRouteStartRecord(sequence, time.Now(), endpoint, requestBytes, decision, cfg)
	emitRouteRecord(record)
	return sequence
}

func makeRouteStartRecord(sequence uint64, now time.Time, endpoint string, requestBytes int, decision router.Decision, cfg config.Config) routeStartRecord {
	chosen := -1
	if len(decision.CandidateState) > 0 {
		chosen = decision.CandidateState[0].Index
	}
	return routeStartRecord{
		Version:           routeJournalVersion,
		Event:             "start",
		Sequence:          sequence,
		UnixMS:            now.UnixMilli(),
		Endpoint:          endpoint,
		RequestBytes:      requestBytes,
		TotalBlocks:       decision.TotalBlocks,
		Chosen:            chosen,
		Outcome:           decision.Outcome,
		Rotation:          decision.Rotation,
		Alpha:             cfg.RouteAlpha,
		MaxAffinityBlocks: cfg.RouteMaxOverlapBlocks,
		ChunkBytes:        cfg.RouteChunkBytes,
		LoadUnitBytes:     cfg.RouteLoadUnitBytes,
		MaxLoadUnits:      cfg.RouteMaxLoadUnits,
		ScoreTieBreak:     "overlap",
		Candidates:        decision.CandidateState,
	}
}

func (j *routeJournal) finish(sequence uint64, started, firstByteAt, firstTokenAt time.Time, result string, upstream, status int, responseBytes int64, accumulator *usage.Accumulator) {
	if j == nil || !j.enabled || sequence == 0 {
		return
	}
	record := makeRouteFinishRecord(sequence, time.Now(), started, firstByteAt, firstTokenAt, result, upstream, status, responseBytes, accumulator)
	emitRouteRecord(record)
}

func makeRouteFinishRecord(sequence uint64, now, started, firstByteAt, firstTokenAt time.Time, result string, upstream, status int, responseBytes int64, accumulator *usage.Accumulator) routeFinishRecord {
	record := routeFinishRecord{
		Version:       routeJournalVersion,
		Event:         "finish",
		Sequence:      sequence,
		UnixMS:        now.UnixMilli(),
		Result:        result,
		Upstream:      upstream,
		Status:        status,
		DurationMS:    float64(now.Sub(started).Microseconds()) / 1000,
		ResponseBytes: responseBytes,
	}
	if !firstByteAt.IsZero() {
		value := float64(firstByteAt.Sub(started).Microseconds()) / 1000
		record.FirstByteMS = &value
	}
	if !firstTokenAt.IsZero() {
		value := float64(firstTokenAt.Sub(started).Microseconds()) / 1000
		record.TTFTMS = &value
	}
	if accumulator != nil {
		record.PromptTokens = accumulator.Prompt
		record.CachedTokens = accumulator.Cached
		record.CompletionTokens = accumulator.Completion
	}
	return record
}

func emitRouteRecord(record any) {
	encoded, err := json.Marshal(record)
	if err != nil {
		log.Printf("[route_journal_error] %v", err)
		return
	}
	log.Printf("[route_journal] %s", encoded)
}
