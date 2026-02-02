package config

import "time"

// AgentConfig is a minimal subset of the configuration used by the Pantheon MCP client/runtime.
// Keep it intentionally small for agent0's MVP controller.
type AgentConfig struct {
	MCPBaseURL        string
	PollInitial       time.Duration
	PollMax           time.Duration
	PollTimeout       time.Duration
	PollBackoffFactor float64

	ProjectName  string
	WorkspaceDir string
}

