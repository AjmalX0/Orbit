// Go part of examples/polyglot.orbit: text.frequencies.
package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"sort"
	"syscall"

	orbit "github.com/AjmalX0/Orbit/sdks/go"
)

type input struct {
	Tokens []string `json:"tokens"`
	Top    int      `json:"top"`
}

type wordCount struct {
	Word  string `json:"word"`
	Count int    `json:"count"`
}

func main() {
	w := orbit.NewWorker(orbit.Options{WorkerID: "text-go"})
	w.Task("text.frequencies", "1.0.0", func(ctx context.Context, t *orbit.Task) (any, error) {
		var in input
		if err := t.Decode(&in); err != nil {
			return nil, orbit.Permanent("INVALID_INPUT", err.Error())
		}
		counts := map[string]int{}
		for _, tok := range in.Tokens {
			counts[tok]++
		}
		top := make([]wordCount, 0, len(counts))
		for word, n := range counts {
			top = append(top, wordCount{word, n})
		}
		sort.Slice(top, func(i, j int) bool {
			if top[i].Count != top[j].Count {
				return top[i].Count > top[j].Count
			}
			return top[i].Word < top[j].Word
		})
		if in.Top >= 0 && len(top) > in.Top {
			top = top[:in.Top]
		}
		return map[string]any{"top": top, "unique": len(counts)}, nil
	})
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if err := w.Run(ctx); err != nil {
		log.Fatal(err)
	}
}
