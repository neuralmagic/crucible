// The report example's scatter plot, as a view plugin written in Go.
//
//	GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared -o ../report-go.wasm .
package main

import (
	"encoding/json"
	"strings"
	"unsafe"
)

type request struct {
	Files []struct {
		Path string `json:"path"`
		Task string `json:"task"`
		Text string `json:"text"`
	} `json:"files"`
}

type record struct {
	AgeDays float64 `json:"age_days"`
	Verdict struct {
		Verdict string `json:"verdict"`
		Actions struct {
			Lane string `json:"lane"`
			CVSS *struct {
				Score float64 `json:"score"`
			} `json:"cvss"`
		} `json:"proposed_actions"`
		Rationale string `json:"rationale"`
	} `json:"verdict_property"`
}

type series struct {
	Label  string       `json:"label"`
	Tone   string       `json:"tone"`
	Points [][2]float64 `json:"points"`
}

type category struct{ label, tone string }

var categories = []category{
	{"affected", "bad"},
	{"needs_review", "warn"},
	{"sbom_inaccurate", "info"},
	{"already_fixed", "good"},
	{"not_shipped", "muted"},
	{"not_affected", "good"},
}

func categorize(r record) string {
	v := r.Verdict
	closeAs := ""
	for _, line := range strings.Split(v.Rationale, "\n") {
		if rest, ok := strings.CutPrefix(line, "Close recommendation:"); ok {
			closeAs = strings.TrimSpace(rest)
		}
	}
	scanAbsent := strings.Contains(v.Rationale, "final-artifact package scan for") &&
		strings.Contains(v.Rationale, "absent")
	switch {
	case closeAs == "not_shipped":
		return "not_shipped"
	case v.Actions.Lane == "absent" || scanAbsent:
		return "sbom_inaccurate"
	case closeAs == "already_fixed":
		return "already_fixed"
	case v.Verdict == "affected":
		return "affected"
	case v.Verdict == "not_affected":
		return "not_affected"
	}
	return "needs_review"
}

func view(input []byte) (any, error) {
	var req request
	if err := json.Unmarshal(input, &req); err != nil {
		return nil, err
	}
	points := map[string][][2]float64{}
	for _, file := range req.Files {
		var records []record
		if err := json.Unmarshal([]byte(file.Text), &records); err != nil {
			return nil, err
		}
		for _, r := range records {
			if cvss := r.Verdict.Actions.CVSS; cvss != nil {
				c := categorize(r)
				points[c] = append(points[c], [2]float64{cvss.Score, r.AgeDays})
			}
		}
	}
	all := make([]series, 0, len(categories))
	for _, c := range categories {
		p := points[c.label]
		if p == nil {
			p = [][2]float64{}
		}
		all = append(all, series{c.label, c.tone, p})
	}
	return map[string]any{
		"title": "Days open by score (Go)",
		"body": map[string]any{"plot": map[string]any{
			"kind": "scatter", "x": "CVSS score", "y": "days open", "series": all,
		}},
	}, nil
}

var kept [][]byte

func handBack(v any) uint64 {
	out, err := json.Marshal(v)
	if err != nil {
		out = []byte(`{"error":"encoding the reply"}`)
	}
	kept = append(kept, out)
	return uint64(uintptr(unsafe.Pointer(unsafe.SliceData(out))))<<32 | uint64(len(out))
}

//go:wasmexport crucible_alloc
func alloc(n uint32) uint32 {
	buf := make([]byte, n)
	kept = append(kept, buf)
	return uint32(uintptr(unsafe.Pointer(unsafe.SliceData(buf))))
}

//go:wasmexport crucible_inputs
func inputs() uint64 { return handBack([]string{"verdicts.json"}) }

//go:wasmexport crucible_view
func build(ptr, n uint32) uint64 {
	input := unsafe.Slice((*byte)(unsafe.Pointer(uintptr(ptr))), n)
	v, err := view(input)
	if err != nil {
		return handBack(map[string]string{"error": err.Error()})
	}
	return handBack(map[string]any{"view": v})
}

func main() {}
