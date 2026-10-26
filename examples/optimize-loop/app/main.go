// The benchmark harness. The verifier pins this file: edit topk.go instead.
//
// It generates a log of access records from a seed salted with the pod's hostname, runs
// TopUsers over a fresh copy of them `iterations` times, and prints one line:
//
//	optimize-loop-result {"pod": ..., "seed": ..., "checksum": ..., "ops_per_s": ..., ...}
//
// The checksum is the SHA-256 of TopUsers' answer, which the verifier recomputes from the pod
// name alone, so a variant that gets faster by getting the answer wrong does not count.
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"runtime"
	"slices"
	"strconv"
	"time"
)

const (
	defaultRecords = 200_000
	users          = 5_000
	topK           = 20
	iterations     = 5
	baseSeed       = 0x6f70746c6f6f70
)

// Record is one access: who, and how many bytes they moved.
type Record struct {
	User  uint32
	Bytes uint32
}

// UserCount is one user's total.
type UserCount struct {
	User  string
	Count int
	Bytes uint64
}

type result struct {
	Variant    string  `json:"variant"`
	Pod        string  `json:"pod"`
	Seed       string  `json:"seed"`
	Records    int     `json:"records"`
	Iterations int     `json:"iterations"`
	P50Ms      float64 `json:"p50_ms"`
	OpsPerS    float64 `json:"ops_per_s"`
	TotalMs    float64 `json:"total_ms"`
	Checksum   string  `json:"checksum"`
	ExeSha256  string  `json:"exe_sha256"`
	Go         string  `json:"go"`
	Procs      int     `json:"gomaxprocs"`
}

func fnv1a64(s string) uint64 {
	h := uint64(0xcbf29ce484222325)
	for i := 0; i < len(s); i++ {
		h ^= uint64(s[i])
		h *= 0x100000001b3
	}
	return h
}

type splitmix64 uint64

func (s *splitmix64) next() uint64 {
	*s += 0x9e3779b97f4a7c15
	z := uint64(*s)
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

func generate(seed uint64, n int) []Record {
	rng := splitmix64(seed)
	out := make([]Record, n)
	for i := range out {
		out[i] = Record{User: uint32(rng.next() % users), Bytes: uint32(rng.next() % 4096)}
	}
	return out
}

func checksum(top []UserCount) string {
	h := sha256.New()
	for _, u := range top {
		fmt.Fprintf(h, "%s %d %d\n", u.User, u.Count, u.Bytes)
	}
	return hex.EncodeToString(h.Sum(nil))
}

func exeSha256() (string, error) {
	path, err := os.Executable()
	if err != nil {
		return "", err
	}
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

func run() error {
	start := time.Now()
	pod, err := os.Hostname()
	if err != nil {
		return fmt.Errorf("reading the hostname: %w", err)
	}
	records := defaultRecords
	if raw := os.Getenv("BENCH_RECORDS"); raw != "" {
		if records, err = strconv.Atoi(raw); err != nil || records < 1 {
			return fmt.Errorf("BENCH_RECORDS=%q is not a positive integer", raw)
		}
	}
	seed := uint64(baseSeed) ^ fnv1a64(pod)
	input := generate(seed, records)
	var want string
	times := make([]float64, 0, iterations)
	for i := 0; i < iterations; i++ {
		fresh := slices.Clone(input)
		t := time.Now()
		top := TopUsers(fresh, topK)
		times = append(times, float64(time.Since(t).Nanoseconds())/1e6)
		got := checksum(top)
		if i == 0 {
			want = got
		} else if got != want {
			return fmt.Errorf("iteration %d answered %s, iteration 0 answered %s", i, got, want)
		}
	}
	slices.Sort(times)
	p50 := times[len(times)/2]
	exe, err := exeSha256()
	if err != nil {
		return fmt.Errorf("hashing the executable: %w", err)
	}
	line, err := json.Marshal(result{
		Variant:    os.Getenv("BENCH_VARIANT"),
		Pod:        pod,
		Seed:       strconv.FormatUint(seed, 10),
		Records:    records,
		Iterations: iterations,
		P50Ms:      p50,
		OpsPerS:    float64(records) / (p50 / 1e3),
		TotalMs:    float64(time.Since(start).Nanoseconds()) / 1e6,
		Checksum:   want,
		ExeSha256:  exe,
		Go:         runtime.Version(),
		Procs:      runtime.GOMAXPROCS(0),
	})
	if err != nil {
		return err
	}
	fmt.Printf("optimize-loop-result %s\n", line)
	return nil
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "bench:", err)
		os.Exit(1)
	}
}
