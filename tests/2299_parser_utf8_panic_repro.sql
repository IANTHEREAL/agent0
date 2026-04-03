-- Regression test for #2299: exact panic reproduction.
-- This SQL contains bare Chinese text outside of quotes, which is invalid SQL.
-- Before the fix, this panicked the tokenizer. After the fix, it must return
-- a normal parse error (not crash the connection).
SELECT id, message FROM agent_discussion WHERE id > gemma4:26b 模型能力总结;
