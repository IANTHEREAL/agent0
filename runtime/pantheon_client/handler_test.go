package tools

import (
	"errors"
	"fmt"
	"strings"
	"testing"
)

func TestExecuteAgentReviewCodeRetriesMissingLog(t *testing.T) {
	client := &fakeMCPClient{
		readResults: []branchReadResult{
			{data: map[string]any{"error": "404: File or directory not found: /workspace/code_review.log"}},
			{err: notFoundErr(2)},
			{data: map[string]any{"content": "ok"}},
		},
	}
	handler := &ToolHandler{
		client:        client,
		defaultProj:   "proj",
		branchTracker: NewBranchTracker("parent"),
		workspaceDir:  "/workspace",
	}

	args := map[string]any{
		"agent":            "review_code",
		"prompt":           "review the latest changes",
		"parent_branch_id": "parent",
		"project_name":     "proj",
	}

	res, err := handler.executeAgent(args)
	if err != nil {
		t.Fatalf("executeAgent returned error: %v", err)
	}

	if got := client.parallelExploreCalls; got != 3 {
		t.Fatalf("expected 3 execute attempts, got %d", got)
	}
	if got := len(client.branchReadInputs); got != 3 {
		t.Fatalf("expected 3 read_artifact attempts, got %d", got)
	}
	for idx, input := range client.branchReadInputs {
		if input.path != "/workspace/code_review.log" {
			t.Fatalf("read attempt %d used path %q", idx+1, input.path)
		}
	}
	if got := res["branch_id"]; got != "branch-3" {
		t.Fatalf("expected final branch_id branch-3, got %#v", got)
	}
	if report, ok := res["review_report"].(string); !ok || strings.TrimSpace(report) != "ok" {
		t.Fatalf("expected review_report=ok, got %#v", res["review_report"])
	}
}

func TestExecuteAgentReviewCodeFailsAfterMaxAttempts(t *testing.T) {
	client := &fakeMCPClient{
		readResults: []branchReadResult{
			{err: notFoundErr(1)},
			{err: notFoundErr(2)},
			{err: notFoundErr(3)},
		},
	}
	handler := &ToolHandler{
		client:        client,
		defaultProj:   "proj",
		branchTracker: NewBranchTracker("parent"),
		workspaceDir:  "/workspace",
	}

	args := map[string]any{
		"agent":            "review_code",
		"prompt":           "review the latest changes",
		"parent_branch_id": "parent",
		"project_name":     "proj",
	}

	_, err := handler.executeAgent(args)
	if err == nil {
		t.Fatalf("expected error after max attempts, got nil")
	}

	var te ToolExecutionError
	if !errors.As(err, &te) {
		t.Fatalf("expected ToolExecutionError, got %T", err)
	}
	if te.Instruction != "FINISHED_WITH_ERROR" {
		t.Fatalf("expected FINISHED_WITH_ERROR instruction, got %q", te.Instruction)
	}
	if te.Details["attempts"] != 3 {
		t.Fatalf("expected attempts=3 in details, got %#v", te.Details["attempts"])
	}
	if !strings.Contains(te.Msg, "branch-3") {
		t.Fatalf("expected error message to mention last branch id, got %q", te.Msg)
	}
}

func TestHandleBranchOutputRequiresBranchID(t *testing.T) {
	handler := &ToolHandler{
		client:        &fakeMCPClient{},
		branchTracker: NewBranchTracker("parent"),
	}
	call := ToolCall{}
	call.Function.Name = "branch_output"
	call.Function.Arguments = "{}"

	res := handler.Handle(call)
	if status := res["status"]; status != "error" {
		t.Fatalf("expected status error, got %#v", status)
	}
	errPayload, _ := res["error"].(map[string]any)
	if errPayload["message"] != "`branch_id` is required" {
		t.Fatalf("expected missing branch_id message, got %#v", errPayload["message"])
	}
}

func TestHandleBranchOutputCallsClient(t *testing.T) {
	client := &fakeMCPClient{
		branchOutputResult: map[string]any{"output": "short"},
	}
	handler := &ToolHandler{
		client:        client,
		branchTracker: NewBranchTracker("parent"),
	}
	call := ToolCall{}
	call.Function.Name = "branch_output"
	call.Function.Arguments = `{"branch_id":"branch-123","full_output":true}`

	res := handler.Handle(call)
	if status := res["status"]; status != "success" {
		t.Fatalf("expected status success, got %#v", status)
	}
	data, _ := res["data"].(map[string]any)
	if data["output"] != "short" {
		t.Fatalf("unexpected data payload %#v", data)
	}
	if len(client.branchOutputInputs) != 1 {
		t.Fatalf("expected 1 branch_output call, got %d", len(client.branchOutputInputs))
	}
	if got := client.branchOutputInputs[0]; got.branchID != "branch-123" || !got.fullOutput {
		t.Fatalf("unexpected branch_output args: %#v", got)
	}
}

func TestHandleBranchOutputDefaultsFullOutputFalse(t *testing.T) {
	client := &fakeMCPClient{
		branchOutputResult: map[string]any{"output": "partial"},
	}
	handler := &ToolHandler{
		client:        client,
		branchTracker: NewBranchTracker("parent"),
	}
	call := ToolCall{}
	call.Function.Name = "branch_output"
	call.Function.Arguments = `{"branch_id":"branch-234"}`

	_ = handler.Handle(call)
	if len(client.branchOutputInputs) != 1 {
		t.Fatalf("expected 1 branch_output call, got %d", len(client.branchOutputInputs))
	}
	if got := client.branchOutputInputs[0]; got.fullOutput {
		t.Fatalf("expected default full_output=false, got true")
	}
}

func TestReadArtifactHandlesErrorPayload(t *testing.T) {
	client := &fakeMCPClient{
		readResults: []branchReadResult{
			{data: map[string]any{"error": "404: File or directory not found: /workspace/missing.log"}},
		},
	}
	handler := &ToolHandler{
		client:        client,
		branchTracker: NewBranchTracker("parent"),
	}
	call := ToolCall{}
	call.Function.Name = "read_artifact"
	call.Function.Arguments = `{"branch_id":"branch-1","path":"/workspace/missing.log"}`

	res := handler.Handle(call)
	if status := res["status"]; status != "error" {
		t.Fatalf("expected status error, got %#v", status)
	}
	errMsg, _ := res["error"].(string)
	if !strings.Contains(errMsg, "404") || !strings.Contains(errMsg, "missing.log") {
		t.Fatalf("unexpected error message %#v", errMsg)
	}
}

type branchReadInput struct {
	branchID string
	path     string
}

type branchReadResult struct {
	data map[string]any
	err  error
}

type fakeMCPClient struct {
	parallelExploreCalls int
	readResults          []branchReadResult
	branchReadInputs     []branchReadInput
	branchOutputInputs   []branchOutputInput
	branchOutputResult   map[string]any
	branchOutputErr      error
}

type branchOutputInput struct {
	branchID   string
	fullOutput bool
}

func (f *fakeMCPClient) ParallelExplore(projectName, parentBranchID string, prompts []string, agent string, numBranches int) (map[string]any, error) {
	f.parallelExploreCalls++
	branchID := fmt.Sprintf("branch-%d", f.parallelExploreCalls)
	return map[string]any{
		"branch_id": branchID,
	}, nil
}

func (f *fakeMCPClient) GetBranch(branchID string) (map[string]any, error) {
	return map[string]any{
		"id":     branchID,
		"status": "succeed",
	}, nil
}

func (f *fakeMCPClient) BranchReadFile(branchID, filePath string) (map[string]any, error) {
	f.branchReadInputs = append(f.branchReadInputs, branchReadInput{branchID: branchID, path: filePath})
	if len(f.readResults) == 0 {
		return nil, fmt.Errorf("no stub result for branch %s", branchID)
	}
	next := f.readResults[0]
	f.readResults = f.readResults[1:]
	if next.err != nil {
		return nil, next.err
	}
	if next.data != nil {
		if errVal, ok := next.data["error"]; ok && errVal != nil {
			switch v := errVal.(type) {
			case string:
				return nil, fmt.Errorf("%s", strings.TrimSpace(v))
			case map[string]any:
				if msg, ok := v["message"].(string); ok && strings.TrimSpace(msg) != "" {
					return nil, fmt.Errorf("%s", strings.TrimSpace(msg))
				}
				return nil, fmt.Errorf("%v", v)
			default:
				return nil, fmt.Errorf("%v", v)
			}
		}
	}
	return next.data, nil
}

func (f *fakeMCPClient) BranchOutput(branchID string, fullOutput bool) (map[string]any, error) {
	f.branchOutputInputs = append(f.branchOutputInputs, branchOutputInput{branchID: branchID, fullOutput: fullOutput})
	if f.branchOutputErr != nil {
		return nil, f.branchOutputErr
	}
	if f.branchOutputResult == nil {
		return map[string]any{"output": "ok"}, nil
	}
	return f.branchOutputResult, nil
}

func notFoundErr(attempt int) error {
	return fmt.Errorf("MCP HTTP 404: attempt %d not found", attempt)
}
