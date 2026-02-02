package tools

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"
)

type stubControllerClient struct {
	parallelExploreCalls int
	branches             []string
	getBranch            func(branchID string) (map[string]any, error)
	branchOutput         func(branchID string, fullOutput bool) (map[string]any, error)
}

func (s *stubControllerClient) ParallelExplore(projectName, parentBranchID string, prompts []string, agent string, numBranches int) (map[string]any, error) {
	s.parallelExploreCalls++
	id := ""
	if len(s.branches) > 0 {
		id = s.branches[0]
		s.branches = s.branches[1:]
	} else {
		id = "branch-" + string(rune('0'+s.parallelExploreCalls))
	}
	return map[string]any{"branch_id": id}, nil
}

func (s *stubControllerClient) GetBranch(branchID string) (map[string]any, error) {
	if s.getBranch != nil {
		return s.getBranch(branchID)
	}
	return map[string]any{"id": branchID, "status": "succeed"}, nil
}

func (s *stubControllerClient) BranchReadFile(branchID, filePath string) (map[string]any, error) {
	return map[string]any{"content": ""}, nil
}

func (s *stubControllerClient) BranchOutput(branchID string, fullOutput bool) (map[string]any, error) {
	if s.branchOutput != nil {
		return s.branchOutput(branchID, fullOutput)
	}
	return map[string]any{"output": "ok"}, nil
}

func TestControllerPromotesAnchorOnSuccess(t *testing.T) {
	tmp := t.TempDir()
	statePath := filepath.Join(tmp, "state.json")

	client := &stubControllerClient{
		branches: []string{"branch-1", "branch-2"},
		getBranch: func(branchID string) (map[string]any, error) {
			return map[string]any{"id": branchID, "status": "succeed"}, nil
		},
		branchOutput: func(branchID string, fullOutput bool) (map[string]any, error) {
			return map[string]any{"output": "hello"}, nil
		},
	}

	cfg := ControllerConfig{
		MCPBaseURL:     "http://localhost:8000/mcp/sse",
		ProjectName:    "proj",
		ParentBranchID: "parent-0",
		Task:           "do it",
		StatePath:      statePath,
		MaxEpisodes:    1,
	}

	err := runControllerWithClient(context.Background(), cfg, client, func(time.Duration) {})
	if err != nil {
		t.Fatalf("expected nil error, got %v", err)
	}

	data, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatalf("read state file: %v", err)
	}
	var st ControllerState
	if err := json.Unmarshal(data, &st); err != nil {
		t.Fatalf("parse state file: %v", err)
	}
	// First run always bootstraps once, then runs episode 1.
	if st.AnchorBranch != "branch-2" {
		t.Fatalf("expected anchor branch-2, got %q", st.AnchorBranch)
	}
	if st.ActiveBranch != "" {
		t.Fatalf("expected active branch cleared, got %q", st.ActiveBranch)
	}
	if !st.Initialized {
		t.Fatalf("expected initialized=true after bootstrap")
	}
	if st.BootstrapBranch != "branch-1" {
		t.Fatalf("expected bootstrap_branch_id=branch-1, got %q", st.BootstrapBranch)
	}
	if client.parallelExploreCalls != 2 {
		t.Fatalf("expected 2 parallel_explore calls (bootstrap + episode), got %d", client.parallelExploreCalls)
	}
}

func TestControllerRetriesFailedThreeTimesThenExits(t *testing.T) {
	tmp := t.TempDir()
	statePath := filepath.Join(tmp, "state.json")

	client := &stubControllerClient{
		branches: []string{"branch-1", "branch-2", "branch-3"},
		getBranch: func(branchID string) (map[string]any, error) {
			return map[string]any{"id": branchID, "status": "failed"}, nil
		},
		branchOutput: func(branchID string, fullOutput bool) (map[string]any, error) {
			return map[string]any{"output": "failed"}, nil
		},
	}

	var sleeps []time.Duration
	sleepFn := func(d time.Duration) { sleeps = append(sleeps, d) }

	cfg := ControllerConfig{
		MCPBaseURL:     "http://localhost:8000/mcp/sse",
		ProjectName:    "proj",
		ParentBranchID: "parent-0",
		Task:           "do it",
		StatePath:      statePath,
	}

	err := runControllerWithClient(context.Background(), cfg, client, sleepFn)
	if err == nil {
		t.Fatalf("expected error, got nil")
	}
	if client.parallelExploreCalls != 3 {
		t.Fatalf("expected 3 parallel_explore calls, got %d", client.parallelExploreCalls)
	}
	if len(sleeps) != 3 {
		t.Fatalf("expected 3 sleeps, got %d", len(sleeps))
	}
	for i, d := range sleeps {
		if d != 20*time.Minute {
			t.Fatalf("sleep %d expected 20m, got %s", i+1, d)
		}
	}

	data, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatalf("read state file: %v", err)
	}
	var st ControllerState
	if err := json.Unmarshal(data, &st); err != nil {
		t.Fatalf("parse state file: %v", err)
	}
	if st.AnchorBranch != "parent-0" {
		t.Fatalf("expected anchor to remain parent-0, got %q", st.AnchorBranch)
	}
	if st.ActiveBranch != "" {
		t.Fatalf("expected active cleared after failures, got %q", st.ActiveBranch)
	}
	if st.Initialized {
		t.Fatalf("expected initialized=false after bootstrap failures")
	}
	if st.BootstrapBranch != "" {
		t.Fatalf("expected bootstrap_branch_id empty after failures, got %q", st.BootstrapBranch)
	}
}

func TestControllerResumesActiveBranch(t *testing.T) {
	tmp := t.TempDir()
	statePath := filepath.Join(tmp, "state.json")

	initial := ControllerState{
		MCPBaseURL:   "http://localhost:8000/mcp/sse",
		ProjectName:  "proj",
		Agent:        "codex",
		Task:         "do it",
		Initialized:  true,
		AnchorBranch: "parent-0",
		ActiveBranch: "branch-77",
	}
	if err := saveControllerState(statePath, initial); err != nil {
		t.Fatalf("save initial state: %v", err)
	}

	client := &stubControllerClient{
		getBranch: func(branchID string) (map[string]any, error) {
			return map[string]any{"id": branchID, "status": "succeed"}, nil
		},
		branchOutput: func(branchID string, fullOutput bool) (map[string]any, error) {
			return map[string]any{"output": "ok"}, nil
		},
	}

	cfg := ControllerConfig{
		StatePath:   statePath,
		MaxEpisodes: 1,
	}

	err := runControllerWithClient(context.Background(), cfg, client, func(time.Duration) {})
	if err != nil {
		t.Fatalf("expected nil error, got %v", err)
	}
	if client.parallelExploreCalls != 0 {
		t.Fatalf("expected 0 parallel_explore calls on resume, got %d", client.parallelExploreCalls)
	}

	data, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatalf("read state file: %v", err)
	}
	var st ControllerState
	if err := json.Unmarshal(data, &st); err != nil {
		t.Fatalf("parse state file: %v", err)
	}
	if st.AnchorBranch != "branch-77" {
		t.Fatalf("expected anchor promoted to branch-77, got %q", st.AnchorBranch)
	}
	if st.ActiveBranch != "" {
		t.Fatalf("expected active cleared, got %q", st.ActiveBranch)
	}
}
