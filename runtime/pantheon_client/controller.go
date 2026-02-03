package tools

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/IANTHEREAL/agent0/internal/logx"
)

const (
	defaultPollTimeoutSeconds     = 24 * 60 * 60
	defaultPollIntervalSeconds    = 60
	defaultMaxPollIntervalSeconds = 300

	bootstrapMCPServerName = "test"
	bootstrapMCPServerURL  = "http://35.89.132.179:8000/mcp/sse"
)

type ControllerConfig struct {
	// MCPBaseURL is the Pantheon MCP SSE endpoint, e.g. http://host:8000/mcp/sse.
	MCPBaseURL string

	ProjectName string
	Agent       string

	// Rebootstrap forces running the bootstrap episode even if the controller
	// state is already initialized.
	Rebootstrap bool

	// ParentBranchID is only used when the state file has no anchor_branch_id yet.
	ParentBranchID string

	// Task is the fixed episode prompt (reused every episode).
	Task string

	// Optional initialization sources. MVP: we just prepend instructions to the prompt.
	AgentsMDURL     string
	SkillsURL       string
	MinibookAccount string

	// StatePath stores anchor_branch_id + active_episode_branch_id and optional config.
	StatePath string

	// MaxEpisodes limits episodes in one run. 0 = infinite.
	MaxEpisodes int
}

type ControllerState struct {
	MCPBaseURL      string `json:"mcp_base_url,omitempty"`
	ProjectName     string `json:"project_name,omitempty"`
	Agent           string `json:"agent,omitempty"`
	Task            string `json:"task,omitempty"`
	AgentsMDURL     string `json:"agents_md_url,omitempty"`
	SkillsURL       string `json:"skills_url,omitempty"`
	MinibookAccount string `json:"minibook_account,omitempty"`
	Initialized     bool   `json:"initialized,omitempty"`
	BootstrapBranch string `json:"bootstrap_branch_id,omitempty"`
	AnchorBranch    string `json:"anchor_branch_id,omitempty"`
	ActiveBranch    string `json:"active_episode_branch_id,omitempty"`

	// Backward compat for older state files. Do not write it back out.
	RPCURL string `json:"rpc_url,omitempty"`
}

func RunController(ctx context.Context, cfg ControllerConfig) error {
	sleepFn := time.Sleep
	if ctx != nil {
		sleepFn = func(d time.Duration) {
			t := time.NewTimer(d)
			defer t.Stop()
			select {
			case <-t.C:
			case <-ctx.Done():
			}
		}
	}
	return runControllerWithClient(ctx, cfg, nil, sleepFn)
}

func runControllerWithClient(ctx context.Context, cfg ControllerConfig, client agentClient, sleepFn func(time.Duration)) error {
	statePath := strings.TrimSpace(cfg.StatePath)
	if statePath == "" {
		statePath = defaultControllerStatePath()
	}

	state, err := loadControllerState(statePath)
	if err != nil {
		return err
	}

	applyControllerOverrides(&state, cfg)
	normalizeControllerDefaults(&state)

	if client == nil {
		client = NewMCPClient(state.MCPBaseURL)
	}

	if cfg.Rebootstrap && strings.TrimSpace(state.ActiveBranch) != "" {
		activeBranch := strings.TrimSpace(state.ActiveBranch)
		running, status, err := isBranchRunning(client, activeBranch)
		if err != nil {
			return fmt.Errorf("check active episode branch %s before rebootstrap: %w", activeBranch, err)
		}
		if running {
			msg := fmt.Sprintf("cannot rebootstrap with an active episode branch still running (active_episode_branch_id=%s)", activeBranch)
			if status != "" {
				msg = fmt.Sprintf("%s (status=%s)", msg, status)
			}
			return fmt.Errorf("%s", msg)
		}
		logx.Infof("Clearing stopped active_episode_branch_id=%s before rebootstrap (status=%s).", activeBranch, status)
		state.ActiveBranch = ""
	}
	bootstrapNeeded := cfg.Rebootstrap || !state.Initialized

	if state.ProjectName == "" {
		return fmt.Errorf("project_name is required (set in state file or pass --pantheon-project-name)")
	}
	if state.AnchorBranch == "" {
		if strings.TrimSpace(cfg.ParentBranchID) == "" {
			return fmt.Errorf("parent_branch_id is required for first run (pass --pantheon-parent-branch-id or set anchor_branch_id in %s)", statePath)
		}
		state.AnchorBranch = strings.TrimSpace(cfg.ParentBranchID)
	}

	if err := saveControllerState(statePath, state); err != nil {
		return err
	}

	maxEpisodes := cfg.MaxEpisodes
	episode := 0
	consecutiveFailed := 0

	handler := &ToolHandler{client: client}

	for {
		if bootstrapNeeded {
			logx.Infof("Bootstrap required (anchor=%s). Running bootstrap episode.", state.AnchorBranch)
		}
		if maxEpisodes > 0 && episode >= maxEpisodes {
			logx.Infof("Reached max_episodes=%d. Exiting.", maxEpisodes)
			return nil
		}
		if ctx != nil && ctx.Err() != nil {
			logx.Infof("Stop requested. Exiting after current episode.")
			return nil
		}

		branchID := strings.TrimSpace(state.ActiveBranch)
		if branchID == "" {
			var prompt string
			if bootstrapNeeded {
				prompt = buildBootstrapPrompt(state)
			} else {
				prompt, err = buildEpisodePrompt(state)
				if err != nil {
					return err
				}
			}

			resp, err := client.ParallelExplore(state.ProjectName, state.AnchorBranch, []string{prompt}, state.Agent, 1)
			if err != nil {
				return fmt.Errorf("parallel_explore failed: %w", err)
			}
			if isErr, ok := resp["isError"].(bool); ok && isErr {
				return fmt.Errorf("parallel_explore returned error: %v", resp["error"])
			}
			branchID = strings.TrimSpace(ExtractBranchID(resp))
			if branchID == "" {
				return fmt.Errorf("missing branch id in parallel_explore response: %v", resp)
			}
			state.ActiveBranch = branchID
			if err := saveControllerState(statePath, state); err != nil {
				return err
			}
		}

		// Poll to terminal status.
		_, err := handler.checkStatus(map[string]any{
			"branch_id":                 branchID,
			"timeout_seconds":           float64(defaultPollTimeoutSeconds),
			"poll_interval_seconds":     float64(defaultPollIntervalSeconds),
			"max_poll_interval_seconds": float64(defaultMaxPollIntervalSeconds),
		})
		if err != nil {
			if isTerminalFailed(err) {
				// Terminal failure: clear active branch (this episode is done), then retry with backoff.
				state.ActiveBranch = ""
				_ = saveControllerState(statePath, state)

				consecutiveFailed++
				logx.Errorf("Episode branch %s failed (attempt %d/3).", branchID, consecutiveFailed)

				if ctx != nil && ctx.Err() != nil {
					return nil
				}

				sleepFn(20 * time.Minute)
				if consecutiveFailed >= 3 {
					return err
				}
				continue
			}

			// Unknown/non-terminal error: keep active branch for resume.
			_ = saveControllerState(statePath, state)
			return err
		}

		// Fetch output; MVP success = we can read branch_output(full=true).
		outResp, outErr := client.BranchOutput(branchID, true)
		if outErr != nil {
			// Do not clear active branch; allow resume to retry branch_output later.
			_ = saveControllerState(statePath, state)
			return outErr
		}

		outputText := ""
		if out, ok := outResp["output"].(string); ok {
			outputText = strings.TrimSpace(out)
		}
		if outputText == "" {
			_ = saveControllerState(statePath, state)
			return fmt.Errorf("branch_output empty for %s", branchID)
		}

		// Promote anchor (no extra success gate in MVP).
		state.AnchorBranch = branchID
		state.ActiveBranch = ""
		if bootstrapNeeded {
			state.Initialized = true
			state.BootstrapBranch = branchID
		}
		if err := saveControllerState(statePath, state); err != nil {
			return err
		}

		consecutiveFailed = 0
		if bootstrapNeeded {
			bootstrapNeeded = false
			logx.Infof("Bootstrap completed. anchor_branch_id=%s", state.AnchorBranch)
			continue
		}

		episode++
		logx.Infof("Episode %d completed. anchor_branch_id=%s", episode, state.AnchorBranch)
	}
}

func buildEpisodePrompt(state ControllerState) (string, error) {
	task := strings.TrimSpace(state.Task)
	if task == "" {
		return "", fmt.Errorf("task is required (pass --task, or keep it in the state file)")
	}

	var prefix []string
	if len(prefix) == 0 {
		return task, nil
	}
	return strings.Join(prefix, "\n") + "\n\n" + task, nil
}

func buildBootstrapPrompt(state ControllerState) string {
	var lines []string
	lines = append(lines, "Bootstrap step: install AGENTS.md, skills configuration and register Minibook account if needed. Do not do any other work.")
	lines = append(lines, "If CLAUDE.md exists in the workspace, delete it by running: rm -f CLAUDE.md")
	lines = append(lines, "")
	if strings.TrimSpace(state.AgentsMDURL) != "" {
		lines = append(lines, fmt.Sprintf("Download AGENTS.md by running: curl -fsSL %q -o AGENTS.md", strings.TrimSpace(state.AgentsMDURL)))
	}
	lines = append(lines, "")
	if strings.TrimSpace(state.SkillsURL) != "" {
		lines = append(lines, fmt.Sprintf("Download skills from %q and install them.", strings.TrimSpace(state.SkillsURL)))
	}
	lines = append(lines, "")
	lines = append(lines, fmt.Sprintf("minibook_account=%q", strings.TrimSpace(state.MinibookAccount)))
	lines = append(lines, "If minibook_account does NOT include an API id/key yet, register a new Minibook account using this nickname and obtain an API key (follow the `$minibook` skill after skills are installed).")
	lines = append(lines, "After registration, write BOTH the registered nickname and the API key into AGENTS.md, and add a clear comment that it must not be modified.")
	lines = append(lines, "")
	lines = append(lines, "Example block to append to AGENTS.md:")
	lines = append(lines, "```md")
	lines = append(lines, "<!-- agent0: minibook credentials (DO NOT EDIT) -->")
	lines = append(lines, "- minibook_nickname: <nickname>")
	lines = append(lines, "- minibook_api_key: <api_key>")
	lines = append(lines, "<!-- /agent0: minibook credentials -->")
	lines = append(lines, "```")
	lines = append(lines, "")
	lines = append(lines, "Finally, output the bootstrap report.")
	return strings.Join(lines, "\n")
}

func applyControllerOverrides(state *ControllerState, cfg ControllerConfig) {
	if strings.TrimSpace(cfg.MCPBaseURL) != "" {
		state.MCPBaseURL = strings.TrimSpace(cfg.MCPBaseURL)
	}
	if strings.TrimSpace(cfg.ProjectName) != "" {
		state.ProjectName = strings.TrimSpace(cfg.ProjectName)
	}
	if strings.TrimSpace(cfg.Agent) != "" {
		state.Agent = strings.TrimSpace(cfg.Agent)
	}
	if strings.TrimSpace(cfg.Task) != "" {
		state.Task = cfg.Task
	}
	if strings.TrimSpace(cfg.AgentsMDURL) != "" {
		state.AgentsMDURL = strings.TrimSpace(cfg.AgentsMDURL)
	}
	if strings.TrimSpace(cfg.SkillsURL) != "" {
		state.SkillsURL = strings.TrimSpace(cfg.SkillsURL)
	}
	if strings.TrimSpace(cfg.MinibookAccount) != "" {
		state.MinibookAccount = strings.TrimSpace(cfg.MinibookAccount)
	}
}

func normalizeControllerDefaults(state *ControllerState) {
	if state.MCPBaseURL == "" && strings.TrimSpace(state.RPCURL) != "" {
		state.MCPBaseURL = strings.TrimSpace(state.RPCURL)
		state.RPCURL = ""
	}
	state.MCPBaseURL = strings.TrimSpace(state.MCPBaseURL)
	if state.MCPBaseURL == "" {
		state.MCPBaseURL = "http://localhost:8000/mcp/sse"
	}
	state.ProjectName = strings.TrimSpace(state.ProjectName)
	state.Agent = strings.TrimSpace(state.Agent)
	if state.Agent == "" {
		state.Agent = "codex"
	}
	state.AgentsMDURL = strings.TrimSpace(state.AgentsMDURL)
	state.SkillsURL = strings.TrimSpace(state.SkillsURL)
	state.MinibookAccount = strings.TrimSpace(state.MinibookAccount)
	state.BootstrapBranch = strings.TrimSpace(state.BootstrapBranch)
	state.AnchorBranch = strings.TrimSpace(state.AnchorBranch)
	state.ActiveBranch = strings.TrimSpace(state.ActiveBranch)
}

func defaultControllerStatePath() string {
	return filepath.Join(".", ".agent0", "controller_state.json")
}

func loadControllerState(path string) (ControllerState, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return ControllerState{}, nil
		}
		return ControllerState{}, err
	}
	var st ControllerState
	if err := json.Unmarshal(data, &st); err != nil {
		return ControllerState{}, fmt.Errorf("parse state file %s: %w", path, err)
	}
	return st, nil
}

func saveControllerState(path string, st ControllerState) error {
	dir := filepath.Dir(path)
	if dir != "." && dir != "" {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return err
		}
	}
	data, err := json.MarshalIndent(st, "", "  ")
	if err != nil {
		return err
	}
	data = append(data, '\n')

	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func isTerminalFailed(err error) bool {
	var te ToolExecutionError
	if !errors.As(err, &te) {
		return false
	}
	if te.Instruction != instructionFinishedWithErr {
		return false
	}
	if te.Details == nil {
		return false
	}
	if v, ok := te.Details["status"].(string); ok && stringsTrimLower(v) == "failed" {
		return true
	}
	return false
}

func isBranchRunning(client agentClient, branchID string) (bool, string, error) {
	resp, err := client.GetBranch(branchID)
	if err != nil {
		return false, "", err
	}
	if errMsg, ok := resp["error"]; ok && errMsg != nil {
		msg := strings.ToLower(fmt.Sprintf("%v", errMsg))
		if strings.Contains(msg, "404") || strings.Contains(msg, "not found") {
			return false, "not_found", nil
		}
		return false, "", fmt.Errorf("GetBranch returned error: %v", errMsg)
	}

	status := stringsLower(resp["status"])
	latestSnapID := stringsLower(resp["latest_snap_id"])
	parentID := stringsLower(resp["parent_id"])
	hasNewSnapshot := true
	if parentID != "" {
		parentResp, err := client.GetBranch(parentID)
		if err != nil {
			hasNewSnapshot = false
		} else if errMsg, ok := parentResp["error"]; ok && errMsg != nil {
			hasNewSnapshot = false
		} else {
			parentLatestSnapID := stringsLower(parentResp["latest_snap_id"])
			if parentLatestSnapID != "" && parentLatestSnapID == latestSnapID {
				hasNewSnapshot = false
			}
		}
	}

	if hasNewSnapshot && isTerminalBranchStatus(status) {
		return false, status, nil
	}
	return true, status, nil
}

func isTerminalBranchStatus(status string) bool {
	switch stringsTrimLower(status) {
	case "succeed", "ready_for_manifest", "finished", "failed", "manifesting":
		return true
	default:
		return false
	}
}
