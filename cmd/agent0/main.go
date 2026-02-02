package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"

	pantheon "github.com/IANTHEREAL/agent0/runtime/pantheon_client"
)

func main() {
	defaultStatePath := defaultControllerStatePath()

	var (
		mcpBaseURL      string
		projectName     string
		parentBranchID  string
		mcpAgent        string
		task            string
		maxEpisodes     int
		agentsMDURL     string
		skillsURL       string
		minibookAccount string
	)

	flag.StringVar(&mcpBaseURL, "mcp-base-url", envOr("MCP_BASE_URL", ""), "Pantheon MCP base URL (e.g. http://host:8000/mcp/sse)")
	flag.StringVar(&projectName, "pantheon-project-name", envFirstNonEmpty("PANTHEON_PROJECT_NAME", "MCP_PROJECT_NAME", "PROJECT_NAME"), "Pantheon project name")
	flag.StringVar(&parentBranchID, "pantheon-parent-branch-id", envFirstNonEmpty("PANTHEON_PARENT_BRANCH_ID", "MCP_PARENT_BRANCH_ID"), "Parent branch id used only for first run (when no anchor exists yet)")
	flag.StringVar(&mcpAgent, "pantheon-agent", envFirstNonEmpty("PANTHEON_AGENT", "MCP_AGENT"), "Pantheon agent name (default: codex)")

	flag.StringVar(&task, "task", "", "Episode prompt text (reused every episode)")
	flag.IntVar(&maxEpisodes, "max-episodes", 0, "Max episodes to run (0 = infinite)")
	flag.StringVar(&agentsMDURL, "agents-md-url", envOr("AGENTS_MD_URL", ""), "Optional: hint URL to initialize AGENTS.md inside the workspace")
	flag.StringVar(&skillsURL, "skills-url", envOr("SKILLS_URL", ""), "Optional: hint URL to initialize skills inside the workspace")
	flag.StringVar(&minibookAccount, "minibook-account", envOr("MINIBOOK_ACCOUNT", ""), "Minibook account to inject into AGENTS.md during bootstrap")

	flag.Parse()

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	cfg := pantheon.ControllerConfig{
		MCPBaseURL:      mcpBaseURL,
		ProjectName:     projectName,
		ParentBranchID:  parentBranchID,
		Agent:           mcpAgent,
		Task:            task,
		AgentsMDURL:     agentsMDURL,
		SkillsURL:       skillsURL,
		MinibookAccount: minibookAccount,
		StatePath:       defaultStatePath,
		MaxEpisodes:     maxEpisodes,
	}

	if err := pantheon.RunController(ctx, cfg); err != nil {
		fmt.Fprintf(os.Stderr, "agent0: %v\n", err)
		os.Exit(1)
	}
}

func defaultControllerStatePath() string {
	return filepath.Join(".", ".agent0", "controller_state.json")
}

func envOr(name, def string) string {
	if v := os.Getenv(name); strings.TrimSpace(v) != "" {
		return v
	}
	return def
}

func envFirstNonEmpty(names ...string) string {
	for _, name := range names {
		if strings.TrimSpace(name) == "" {
			continue
		}
		if v := strings.TrimSpace(os.Getenv(name)); v != "" {
			return v
		}
	}
	return ""
}
