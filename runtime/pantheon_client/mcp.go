package tools

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync/atomic"
	"time"

	"github.com/IANTHEREAL/agent0/internal/logx"
)

type MCPError struct{ Msg string }

func (e MCPError) Error() string { return e.Msg }

type MCPClient struct {
	rpcURL     string
	timeout    time.Duration
	maxRetries int
	sessionID  string
	client     *http.Client
	requestID  int64
	bearerToken string
}

func NewMCPClient(baseURL string, bearerToken string) *MCPClient {
	base := strings.TrimRight(baseURL, "/")
	if base == "" {
		base = "http://localhost:8000/mcp/sse"
	}
	return &MCPClient{
		rpcURL:      base,
		timeout:     30 * time.Second,
		maxRetries:  3,
		sessionID:   fmt.Sprintf("%d", time.Now().UnixNano()),
		client:      &http.Client{},
		bearerToken: strings.TrimSpace(bearerToken),
	}
}

func (c *MCPClient) rpcPost(url string, body map[string]any, timeout time.Duration) (*http.Response, context.CancelFunc, error) {
	payload, _ := json.Marshal(body)
	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(payload))
	if err != nil {
		return nil, nil, err
	}
	req.Header.Set("Accept", "application/json, text/event-stream")
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Mcp-Session-Id", c.sessionID)
	if c.bearerToken != "" {
		req.Header.Set("Authorization", "Bearer "+c.bearerToken)
	}

	effectiveTimeout := timeout
	if effectiveTimeout <= 0 {
		effectiveTimeout = c.timeout
	}

	ctx := context.Background()
	var cancel context.CancelFunc
	if effectiveTimeout > 0 {
		ctx, cancel = context.WithTimeout(ctx, effectiveTimeout)
	} else {
		cancel = func() {}
	}
	req = req.WithContext(ctx)

	resp, err := c.client.Do(req)
	if err != nil {
		cancel()
		return nil, nil, err
	}
	return resp, cancel, nil
}

func (c *MCPClient) call(method string, params map[string]any, timeout time.Duration) (map[string]any, error) {
	return c.callWithRetries(method, params, timeout, c.maxRetries)
}

func (c *MCPClient) callWithRetries(method string, params map[string]any, timeout time.Duration, maxRetries int) (map[string]any, error) {
	if maxRetries < 1 {
		maxRetries = 1
	}
	requestID := atomic.AddInt64(&c.requestID, 1)
	payload := map[string]any{
		"jsonrpc": "2.0",
		"id":      requestID,
		"method":  method,
		"params":  params,
	}
	var lastErr error

	for attempt := 0; attempt < maxRetries; attempt++ {
		logx.Debugf("MCP POST %s attempt %d to %s", method, attempt+1, c.rpcURL)
		resp, cancel, err := c.rpcPost(c.rpcURL, payload, timeout)
		if err != nil {
			lastErr = err
		} else {
			ct := resp.Header.Get("Content-Type")
			if resp.StatusCode < 200 || resp.StatusCode >= 300 {
				body, _ := io.ReadAll(resp.Body)
				resp.Body.Close()
				cancel()
				logx.Errorf("MCP HTTP error %d for %s (CT=%s): %.500s", resp.StatusCode, method, ct, string(body))
				lastErr = fmt.Errorf("MCP HTTP %d: %s", resp.StatusCode, string(body))
			} else if strings.Contains(ct, "text/event-stream") {
				data, preview, err := parseSSEStream(resp.Body)
				resp.Body.Close()
				cancel()
				if preview != "" {
					logx.Debugf("MCP SSE preview: %q", preview)
				}
				if err != nil {
					logx.Errorf("Failed to parse SSE JSON for %s. Content-Type: %s, Status: %d (%v)", method, ct, resp.StatusCode, err)
					lastErr = err
				} else {
					var obj map[string]any
					if err := json.Unmarshal(data, &obj); err != nil {
						logx.Errorf("MCP SSE payload not JSON (status %d, CT=%s). Preview: %.200s", resp.StatusCode, ct, string(data[:min(200, len(data))]))
						lastErr = err
					} else {
						return normalizeRPC(obj), nil
					}
				}
			} else {
				data, err := io.ReadAll(resp.Body)
				resp.Body.Close()
				cancel()
				if err != nil {
					logx.Errorf("Failed reading MCP response body for %s: %v (bytes=%d)", method, err, len(data))
					lastErr = err
					continue
				}
				var obj map[string]any
				if err := json.Unmarshal(data, &obj); err != nil {
					logx.Errorf("MCP response not JSON (status %d, CT=%s). First 1000 bytes: %q", resp.StatusCode, ct, string(data[:min(1000, len(data))]))
					lastErr = err
				} else {
					return normalizeRPC(obj), nil
				}
			}
		}
		if attempt < maxRetries-1 {
			wait := time.Duration(1<<attempt) * time.Second
			logx.Warningf("MCP call %s failed (attempt %d/%d): %v. Retrying in %ds...", method, attempt+1, maxRetries, lastErr, int(wait.Seconds()))
			time.Sleep(wait)
		}
	}
	if lastErr == nil {
		lastErr = MCPError{Msg: "Unknown MCP error"}
	}
	return nil, lastErr
}

func normalizeRPC(obj map[string]any) map[string]any {
	if errVal, ok := obj["error"]; ok {
		_ = errVal
		return obj
	}
	if res, ok := obj["result"].(map[string]any); ok {
		if sc, ok := res["structuredContent"].(map[string]any); ok {
			return sc
		}
		return res
	}
	return obj
}

func (c *MCPClient) CallTool(name string, arguments map[string]any) (map[string]any, error) {
	return c.call("tools/call", map[string]any{"name": name, "arguments": arguments}, c.timeout)
}

func (c *MCPClient) ParallelExplore(projectName, parentBranchID string, prompts []string, agent string, numBranches int) (map[string]any, error) {
	return c.CallTool("parallel_explore", map[string]any{
		"project_name":           projectName,
		"parent_branch_id":       parentBranchID,
		"shared_prompt_sequence": prompts,
		"num_branches":           numBranches,
		"agent":                  agent,
	})
}

func (c *MCPClient) GetBranch(branchID string) (map[string]any, error) {
	retries := c.maxRetries
	if retries < 5 {
		retries = 5
	}
	return c.callWithRetries("tools/call", map[string]any{
		"name":      "get_branch",
		"arguments": map[string]any{"branch_id": branchID},
	}, 300*time.Second, retries)
}

func (c *MCPClient) BranchReadFile(branchID, filePath string) (map[string]any, error) {
	resp, err := c.CallTool("branch_read_file", map[string]any{"branch_id": branchID, "file_path": filePath})
	if err != nil {
		return nil, err
	}
	if resp == nil {
		return nil, fmt.Errorf("branch_read_file returned empty response")
	}
	if errVal, ok := resp["error"]; ok && errVal != nil {
		return nil, payloadError(errVal)
	}
	return resp, nil
}

func (c *MCPClient) BranchOutput(branchID string, fullOutput bool) (map[string]any, error) {
	args := map[string]any{"branch_id": branchID}
	if fullOutput {
		args["full_output"] = true
	}
	return c.CallTool("branch_output", args)
}

func parseSSEStream(r io.Reader) ([]byte, string, error) {
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)

	var (
		current strings.Builder
		total   strings.Builder
		preview strings.Builder
	)

	appendPreview := func(line string) {
		const maxPreview = 2000
		if preview.Len() >= maxPreview {
			return
		}
		if preview.Len()+len(line) > maxPreview {
			line = line[:maxPreview-preview.Len()]
		}
		preview.WriteString(line)
		preview.WriteByte('\n')
	}

	tryDecode := func(text string) ([]byte, bool) {
		text = strings.TrimSpace(text)
		if text == "" || text == "[DONE]" || text == "DONE" {
			return nil, false
		}
		if json.Valid([]byte(text)) {
			return []byte(text), true
		}
		if data, err := extractJSONFromText(text); err == nil {
			return data, true
		}
		return nil, false
	}

	for scanner.Scan() {
		line := scanner.Text()
		appendPreview(line)
		line = strings.TrimRight(line, "\r")
		if line == "" {
			if current.Len() > 0 {
				if data, ok := tryDecode(current.String()); ok {
					return data, preview.String(), nil
				}
				current.Reset()
			}
			if data, ok := tryDecode(total.String()); ok {
				return data, preview.String(), nil
			}
			continue
		}
		if strings.HasPrefix(line, ":") {
			continue
		}
		if i := strings.Index(line, ":"); i >= 0 {
			field := strings.TrimSpace(line[:i])
			value := strings.TrimSpace(line[i+1:])
			if strings.EqualFold(field, "data") {
				current.WriteString(value)
				current.WriteByte('\n')
				total.WriteString(value)
				total.WriteByte('\n')
				if data, ok := tryDecode(current.String()); ok {
					return data, preview.String(), nil
				}
				if data, ok := tryDecode(total.String()); ok {
					return data, preview.String(), nil
				}
			}
		}
	}

	if err := scanner.Err(); err != nil {
		return nil, preview.String(), err
	}
	if current.Len() > 0 {
		if data, ok := tryDecode(current.String()); ok {
			return data, preview.String(), nil
		}
	}
	if total.Len() > 0 {
		if data, ok := tryDecode(total.String()); ok {
			return data, preview.String(), nil
		}
	}
	if data, err := extractJSONFromText(preview.String()); err == nil {
		return data, preview.String(), nil
	}
	return nil, preview.String(), fmt.Errorf("no JSON data event in SSE response")
}

func extractJSONFromText(text string) ([]byte, error) {
	text = strings.TrimSpace(text)
	if text == "" {
		return nil, fmt.Errorf("empty text")
	}
	idx := strings.IndexAny(text, "{[")
	if idx < 0 {
		return nil, fmt.Errorf("no JSON start found")
	}
	dec := json.NewDecoder(strings.NewReader(text[idx:]))
	dec.UseNumber()
	var raw json.RawMessage
	if err := dec.Decode(&raw); err != nil {
		return nil, err
	}
	if len(raw) == 0 {
		return nil, fmt.Errorf("decoded empty JSON")
	}
	return raw, nil
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}

func payloadError(val any) error {
	switch v := val.(type) {
	case string:
		msg := strings.TrimSpace(v)
		if msg == "" {
			return fmt.Errorf("unknown error")
		}
		return fmt.Errorf("%s", msg)
	case map[string]any:
		if msg, ok := v["message"].(string); ok && strings.TrimSpace(msg) != "" {
			return fmt.Errorf("%s", strings.TrimSpace(msg))
		}
		if data, err := json.Marshal(v); err == nil {
			return fmt.Errorf("%s", string(data))
		}
	default:
		if v != nil {
			return fmt.Errorf("%v", v)
		}
	}
	return fmt.Errorf("unknown error")
}
