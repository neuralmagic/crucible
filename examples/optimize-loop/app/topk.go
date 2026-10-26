package main

import "fmt"

// TopUsers returns the k users with the most records, most first. Ties go to the user who moved
// more bytes, then to the smaller user name.
//
// Correct, and slow on purpose.
func TopUsers(records []Record, k int) []UserCount {
	var counts []UserCount
	for _, r := range records {
		name := fmt.Sprintf("user-%05d", r.User)
		found := false
		for i := range counts {
			if counts[i].User == name {
				counts[i].Count++
				counts[i].Bytes += uint64(r.Bytes)
				found = true
				break
			}
		}
		if !found {
			counts = append(counts, UserCount{User: name, Count: 1, Bytes: uint64(r.Bytes)})
		}
	}
	for i := 0; i < len(counts); i++ {
		for j := 0; j+1 < len(counts)-i; j++ {
			if before(counts[j+1], counts[j]) {
				counts[j], counts[j+1] = counts[j+1], counts[j]
			}
		}
	}
	if len(counts) > k {
		counts = counts[:k]
	}
	return counts
}

func before(a, b UserCount) bool {
	if a.Count != b.Count {
		return a.Count > b.Count
	}
	if a.Bytes != b.Bytes {
		return a.Bytes > b.Bytes
	}
	return a.User < b.User
}
