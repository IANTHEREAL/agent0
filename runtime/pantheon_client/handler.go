package tools

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/IANTHEREAL/agent0/internal/config"
	"github.com/IANTHEREAL/agent0/internal/logx"
)

type ToolExecutionError struct {
	Msg         string
	Instruction string
	Details     map[string]any
}

func (e ToolExecutionError) Error() string { return e.Msg }

type agentClient interface {
	ParallelExplore(projectName, parentBranchID string, prompts []string, agent string, numBranches int) (map[string]any, error)
	GetBranch(branchID string) (map[string]any, error)
	BranchReadFile(branchID, filePath string) (map[string]any, error)
	BranchOutput(branchID string, fullOutput bool) (map[string]any, error)
}

var _ agentClient = (*MCPClient)(nil)

const (
	reviewCodeAgent            = "review_code"
	reviewArtifactName         = "code_review.log"
	reviewMaxAttempts          = 3
	instructionFinishedWithErr = "FINISHED_WITH_ERROR"
)

type BranchTracker struct {
	mu     sync.Mutex
	start  string
	latest string
}

func NewBranchTracker(start string) *BranchTracker {
	return &BranchTracker{start: start, latest: start}
}

func (t *BranchTracker) Record(id string) {
	if id == "" {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.start == "" {
		t.start = id
	}
	t.latest = id
}

func (t *BranchTracker) Range() map[string]string {
	t.mu.Lock()
	defer t.mu.Unlock()
	return map[string]string{"start_branch_id": t.start, "latest_branch_id": t.latest}
}

type ToolHandler struct {
	client        agentClient
	cfg           *config.AgentConfig // nil = use defaults (for tests)
	defaultProj   string
	branchTracker *BranchTracker
	workspaceDir  string
}

// NewToolHandler creates a handler without config. Uses hardcoded defaults.
// Kept for backward compatibility with tests.
func NewToolHandler(client agentClient, defaultProject string, startBranch string, workspaceDir string) *ToolHandler {
	return &ToolHandler{
		client:        client,
		cfg:           nil,
		defaultProj:   defaultProject,
		branchTracker: NewBranchTracker(startBranch),
		workspaceDir:  strings.TrimSpace(workspaceDir),
	}
}

// NewToolHandlerWithConfig creates a handler with config. Use this in production.
func NewToolHandlerWithConfig(client agentClient, cfg *config.AgentConfig, startBranch string) *ToolHandler {
	return &ToolHandler{
		client:        client,
		cfg:           cfg,
		defaultProj:   cfg.ProjectName,
		branchTracker: NewBranchTracker(startBranch),
		workspaceDir:  strings.TrimSpace(cfg.WorkspaceDir),
	}
}

func (h *ToolHandler) BranchRange() map[string]string { return h.branchTracker.Range() }

// ToolCall mirrors brain.ToolCall, but we keep it generic here if needed.
type ToolCall struct {
	ID       string `json:"id"`
	Type     string `json:"type"`
	Function struct {
		Name      string `json:"name"`
		Arguments string `json:"arguments"`
	} `json:"function"`
}

func (h *ToolHandler) Handle(call ToolCall) map[string]any {
	name := call.Function.Name
	if name == "" {
		return h.errorPayload(ToolExecutionError{Msg: "Missing tool name in call."})
	}
	var args map[string]any
	if call.Function.Arguments != "" {
		if err := json.Unmarshal([]byte(call.Function.Arguments), &args); err != nil {
			return h.errorPayload(ToolExecutionError{Msg: fmt.Sprintf("Invalid JSON arguments: %v", err)})
		}
	} else {
		args = map[string]any{}
	}

	var res map[string]any
	var err error
	switch name {
	case "execute_agent":
		res, err = h.executeAgent(args)
	case "check_status":
		res, err = h.checkStatus(args)
	case "read_artifact":
		res, err = h.readArtifact(args)
	case "branch_output":
		res, err = h.branchOutput(args)
	default:
		err = ToolExecutionError{Msg: fmt.Sprintf("Unsupported tool: %s", name)}
	}
	if err != nil {
		return h.errorPayload(err)
	}
	return map[string]any{"status": "success", "data": res}
}

func (h *ToolHandler) executeAgent(arguments map[string]any) (map[string]any, error) {
	agent, _ := arguments["agent"].(string)
	prompt, _ := arguments["prompt"].(string)
	project := h.defaultProj
	if v, ok := arguments["project_name"].(string); ok && v != "" {
		project = v
	}
	parent, _ := arguments["parent_branch_id"].(string)

	if agent == "" || prompt == "" || parent == "" || project == "" {
		return nil, ToolExecutionError{Msg: "missing required arguments"}
	}

	if agent == reviewCodeAgent {
		return h.executeReviewAgent(project, parent, prompt)
	}
	result, _, err := h.runAgentOnce(agent, project, parent, prompt)
	return result, err
}

func (h *ToolHandler) runAgentOnce(agent, project, parent, prompt string) (map[string]any, string, error) {
	logx.Infof("Executing agent %s on project %s from parent %s", agent, project, parent)
	resp, err := h.client.ParallelExplore(project, parent, []string{prompt}, agent, 1)
	if err != nil {
		return nil, "", ToolExecutionError{
			Msg:         fmt.Sprintf("ParallelExplore failed: %v - %v", err, resp),
			Instruction: instructionFinishedWithErr,
		}
	}
	if isErr, ok := resp["isError"].(bool); ok && isErr {
		errMsg := resp["error"]
		if errMsg == nil {
			return nil, "", ToolExecutionError{
				Msg:         fmt.Sprintf("ParallelExplore returned error (details: %v)", resp),
				Instruction: instructionFinishedWithErr,
			}
		}
		return nil, "", ToolExecutionError{
			Msg:         fmt.Sprintf("ParallelExplore returned error: %v", errMsg),
			Instruction: instructionFinishedWithErr,
		}
	}
	branchID := ExtractBranchID(resp)
	if branchID == "" {
		return nil, "", ToolExecutionError{
			Msg:         fmt.Sprintf("Missing branch id in parallel_explore response: %v", resp),
			Instruction: instructionFinishedWithErr,
		}
	}
	// Don't record branch ID yet - wait until checkStatus succeeds

	result := map[string]any{"parallel_explore": resp, "branch_id": branchID}

	logx.Infof("Waiting for branch %s to complete.", branchID)
	statusResp, err := h.checkStatus(map[string]any{"branch_id": branchID})
	if err != nil {
		// checkStatus failed - don't record this branch ID
		if te, ok := err.(ToolExecutionError); ok {
			// If checkStatus already set FINISHED_WITH_ERROR, propagate it
			if te.Instruction != "" {
				return nil, "", te
			}
			// Otherwise, add the instruction to stop workflow
			te.Instruction = instructionFinishedWithErr
			return nil, "", te
		}
		return nil, "", ToolExecutionError{
			Msg:         fmt.Sprintf("Branch status check failed: %v", err),
			Instruction: instructionFinishedWithErr,
		}
	}

	// Only record branch ID after successful status check
	h.branchTracker.Record(branchID)

	result["branch"] = statusResp
	if status, ok := statusResp["status"]; ok {
		result["status"] = status
	}

	responseText := ""
	if out, ok := statusResp["output"].(string); ok && strings.TrimSpace(out) != "" {
		responseText = strings.TrimSpace(out)
	} else if manifest, ok := statusResp["manifest"].(map[string]any); ok {
		if summary, ok := manifest["summary"].(string); ok && strings.TrimSpace(summary) != "" {
			responseText = strings.TrimSpace(summary)
		}
	}

	branchOutputResponse, err := h.client.BranchOutput(branchID, true)
	if err != nil {
		return nil, "", err
	} else {
		branchOutput := branchOutputString(branchOutputResponse)
		if branchOutput != "" {
			responseText = branchOutput
		}
	}
	if strings.TrimSpace(responseText) == "" {
		return nil, "", ToolExecutionError{Msg: "branch_output returned no textual output"}
	}
	result["response"] = strings.TrimSpace(responseText)

	return result, branchID, nil
}

func (h *ToolHandler) executeReviewAgent(project, parent, prompt string) (map[string]any, error) {
	artifactPath := h.reviewLogPath()
	if artifactPath == "" {
		return nil, ToolExecutionError{Msg: "workspace directory not configured for review_code validation"}
	}
	var lastBranch string
	for attempt := 1; attempt <= reviewMaxAttempts; attempt++ {
		result, branchID, err := h.runAgentOnce(reviewCodeAgent, project, parent, prompt)
		if err != nil {
			return nil, err
		}
		lastBranch = branchID
		if artifact, err := h.client.BranchReadFile(branchID, artifactPath); err == nil {
			if content, ok := artifact["content"].(string); ok && strings.TrimSpace(content) != "" {
				result["review_report"] = content
			}
			return result, nil
		} else if !isNotFoundError(err) {
			return nil, err
		}
		logx.Warningf("review_code attempt %d/%d did not produce %s (branch=%s)", attempt, reviewMaxAttempts, artifactPath, branchID)
	}
	details := map[string]any{
		"attempts":      reviewMaxAttempts,
		"artifact_path": artifactPath,
	}
	if lastBranch != "" {
		details["last_branch_id"] = lastBranch
	}
	msg := fmt.Sprintf("review_code failed to produce %s after %d attempts", artifactPath, reviewMaxAttempts)
	if lastBranch != "" {
		msg = fmt.Sprintf("%s (last_branch_id=%s). Inspect manifest %s in Pantheon.", msg, lastBranch, lastBranch)
	}
	return nil, ToolExecutionError{
		Msg:         msg,
		Instruction: instructionFinishedWithErr,
		Details:     details,
	}
}

func (h *ToolHandler) reviewLogPath() string {
	if strings.TrimSpace(h.workspaceDir) == "" {
		return ""
	}
	return filepath.Join(h.workspaceDir, reviewArtifactName)
}

func (h *ToolHandler) checkStatus(arguments map[string]any) (map[string]any, error) {
	branchID, _ := arguments["branch_id"].(string)
	if branchID == "" {
		return nil, ToolExecutionError{Msg: "`branch_id` is required"}
	}
	// Defaults for tests or when config is nil
	timeout := 3600.0 * 6 // 6 hours
	poll := 3.0
	maxPoll := 30.0
	backoffFactor := 1.5

	// Use config values if available
	if h.cfg != nil {
		timeout = h.cfg.PollTimeout.Seconds()
		poll = h.cfg.PollInitial.Seconds()
		maxPoll = h.cfg.PollMax.Seconds()
		backoffFactor = h.cfg.PollBackoffFactor
	}

	// Tool arguments can override config
	if v, ok := arguments["timeout_seconds"].(float64); ok && v > 0 {
		timeout = v
	}
	if v, ok := arguments["poll_interval_seconds"].(float64); ok && v > 0 {
		poll = v
	}
	if v, ok := arguments["max_poll_interval_seconds"].(float64); ok && v >= poll {
		maxPoll = v
	}
	deadline := time.Now().Add(time.Duration(timeout) * time.Second)
	sleep := time.Duration(poll * float64(time.Second))

	logx.Infof("Checking status for branch %s (timeout=%ds)", branchID, int(timeout))
	for attempt := 1; ; attempt++ {
		resp, err := h.client.GetBranch(branchID)
		if err != nil {
			return nil, ToolExecutionError{
				Msg: fmt.Sprintf("GetBranch API call failed for branch %s: %v", branchID, err),
			}
		}

		// Check if the response contains an error (e.g., 404 branch not found)
		if errMsg, ok := resp["error"]; ok {
			return nil, ToolExecutionError{
				Msg: fmt.Sprintf("GetBranch returned error for branch %s: %v", branchID, errMsg),
			}
		}

		// Validate branch id in response
		if id := ExtractBranchID(resp); id != "" {
			// Don't record here - let the caller decide when to record
			// h.branchTracker.Record(id)
		} else {
			return nil, ToolExecutionError{
				Msg: fmt.Sprintf("Branch status response missing branch identifier. Response: %v", resp),
			}
		}

		status := stringsLower(resp["status"])
		latest_snap_id := stringsLower(resp["latest_snap_id"])
		hasNewSnapshot := true
		parent_branch_id := stringsLower(resp["parent_id"])
		if parent_branch_id != "" {
			parent_resp, err := h.client.GetBranch(parent_branch_id)
			if err != nil {
				logx.Errorf("Error getting parent branch %s: %v", parent_branch_id, err)
				hasNewSnapshot = false
			} else {
				parent_latest_snap_id := stringsLower(parent_resp["latest_snap_id"])
				// If child's latest_snap_id matches parent's, the child is still using the inherited snapshot.
				// The status we see might be inherited from parent, not the child's own status.
				// We must wait for the child to create its own snapshot before trusting the status.
				if parent_latest_snap_id != "" && parent_latest_snap_id == latest_snap_id {
					hasNewSnapshot = false
				}
			}
		}

		logx.Infof("Branch %s response (attempt %d): %s", branchID, attempt, toJSON(resp))
		if hasNewSnapshot && (status == "succeed" || status == "ready_for_manifest" || status == "finished" || status == "failed" || status == "manifesting") {
			if status == "failed" {
				details := map[string]any{"status": status}
				if branchID := ExtractBranchID(resp); branchID != "" {
					details["branch_id"] = branchID
				}
				excerpt := ""
				if outResp, err := h.client.BranchOutput(branchID, true); err == nil {
					excerpt = strings.TrimSpace(branchOutputString(outResp))
					if len(excerpt) > 400 {
						excerpt = excerpt[:400] + "..."
					}
				}
				msg := fmt.Sprintf("Branch %s reported failed status. Inspect manifest %s in Pantheon.", branchID, branchID)
				if excerpt != "" {
					msg = fmt.Sprintf("Branch %s reported failed status: %s. Inspect manifest %s in Pantheon.", branchID, excerpt, branchID)
				}
				return nil, ToolExecutionError{
					Msg:         msg,
					Instruction: instructionFinishedWithErr,
					Details:     details,
				}
			}
			return resp, nil
		}

		if time.Now().After(deadline) {
			return nil, ToolExecutionError{
				Msg:         fmt.Sprintf("Timed out waiting for branch %s (last status=%s)", branchID, status),
				Instruction: instructionFinishedWithErr,
			}
		}
		logx.Infof("Branch %s still active (status=%s). Sleeping %.1fs.", branchID, status, sleep.Seconds())
		time.Sleep(sleep)
		sleep = time.Duration(minFloat(float64(sleep/time.Second)*backoffFactor, maxPoll)) * time.Second
	}
}

func (h *ToolHandler) readArtifact(arguments map[string]any) (map[string]any, error) {
	branchID, _ := arguments["branch_id"].(string)
	path, _ := arguments["path"].(string)
	if branchID == "" || path == "" {
		return nil, ToolExecutionError{Msg: "`branch_id` and `path` are required"}
	}
	logx.Infof("Reading artifact %s from branch %s", path, branchID)
	return h.client.BranchReadFile(branchID, path)
}

func (h *ToolHandler) branchOutput(arguments map[string]any) (map[string]any, error) {
	rawBranchID, _ := arguments["branch_id"].(string)
	branchID := strings.TrimSpace(rawBranchID)
	if branchID == "" {
		return nil, ToolExecutionError{Msg: "`branch_id` is required"}
	}
	fullOutput := false
	if v, ok := arguments["full_output"]; ok {
		flag, ok := v.(bool)
		if !ok {
			return nil, ToolExecutionError{Msg: "`full_output` must be a boolean"}
		}
		fullOutput = flag
	}
	logx.Infof("Retrieving branch_output for %s (full_output=%t)", branchID, fullOutput)
	return h.client.BranchOutput(branchID, fullOutput)
}

func ExtractBranchID(m map[string]any) string {
	if m == nil {
		return ""
	}

	if pe, ok := m["parallel_explore"].(map[string]any); ok {
		if branches, ok := pe["branches"].([]any); ok {
			for _, item := range branches {
				if nested, _ := item.(map[string]any); nested != nil {
					if id := ExtractBranchID(nested); id != "" {
						return id
					}
				}
			}
		}
	}
	if branches, ok := m["branches"].([]any); ok {
		for _, item := range branches {
			if nested, _ := item.(map[string]any); nested != nil {
				if id := ExtractBranchID(nested); id != "" {
					return id
				}
			}
		}
	}
	if b, ok := m["branch"].(map[string]any); ok {
		if id := ExtractBranchID(b); id != "" {
			return id
		}
	}
	for _, k := range []string{"branch_id", "id"} {
		if v, ok := m[k].(string); ok && v != "" {
			return v
		}
	}
	return ""
}

func branchOutputString(payload map[string]any) string {
	if payload == nil {
		return ""
	}
	if out, ok := payload["output"].(string); ok {
		return strings.TrimSpace(out)
	}
	return ""
}

func (h *ToolHandler) errorPayload(err error) map[string]any {
	if err == nil {
		return map[string]any{"status": "error", "error": "unknown error"}
	}
	if te, ok := err.(ToolExecutionError); ok {
		payload := map[string]any{}
		if strings.TrimSpace(te.Msg) != "" {
			payload["message"] = strings.TrimSpace(te.Msg)
		}
		if te.Instruction != "" {
			payload["instruction"] = te.Instruction
		}
		if len(te.Details) > 0 {
			payload["details"] = te.Details
		}
		if len(payload) == 0 {
			payload["message"] = "tool execution error"
		}
		return map[string]any{"status": "error", "error": payload}
	}
	return map[string]any{"status": "error", "error": err.Error()}
}

func stringsLower(v any) string {
	if v == nil {
		return ""
	}
	s, _ := v.(string)
	return stringsTrimLower(s)
}

func stringsTrimLower(s string) string {
	return strings.ToLower(strings.TrimSpace(s))
}

func minFloat(a, b float64) float64 {
	if a < b {
		return a
	}
	return b
}

func isNotFoundError(err error) bool {
	if err == nil {
		return false
	}
	msg := strings.ToLower(err.Error())
	return strings.Contains(msg, "404")
}

// Tool schema to feed the LLM
func GetToolDefinitions() []map[string]any {
	return []map[string]any{
		{
			"type": "function",
			"function": map[string]any{
				"name":        "execute_agent",
				"description": "Launch an MCP parallel_explore job for a specialist agent.",
				"parameters": map[string]any{
					"type": "object",
					"properties": map[string]any{
						"agent":            map[string]any{"type": "string", "description": "Target specialist agent name."},
						"prompt":           map[string]any{"type": "string", "description": "Prompt for the agent."},
						"project_name":     map[string]any{"type": "string", "description": "Pantheon project name."},
						"parent_branch_id": map[string]any{"type": "string", "description": "Branch UUID to branch from."},
					},
					"required": []any{"agent", "prompt", "project_name", "parent_branch_id"},
				},
			},
		},
		{
			"type": "function",
			"function": map[string]any{
				"name":        "read_artifact",
				"description": "Read a text artifact produced by a branch.",
				"parameters": map[string]any{
					"type": "object",
					"properties": map[string]any{
						"branch_id": map[string]any{"type": "string", "description": "Branch that produced the artifact."},
						"path":      map[string]any{"type": "string", "description": "Artifact path or filename."},
					},
					"required": []any{"branch_id", "path"},
				},
			},
		},
		{
			"type": "function",
			"function": map[string]any{
				"name":        "branch_output",
				"description": "Retrieve the text output that a branch produced.",
				"parameters": map[string]any{
					"type": "object",
					"properties": map[string]any{
						"branch_id":   map[string]any{"type": "string", "description": "Branch that produced the output."},
						"full_output": map[string]any{"type": "boolean", "description": "Return the complete output log instead of any default truncation."},
					},
					"required": []any{"branch_id"},
				},
			},
		},
	}
}

func toJSON(v any) string { b, _ := json.Marshal(v); return string(b) }
