-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown exposes only the supported public GUC.
SHOW db9.enable_cop_pushdown;
SHOW db9.enable_cop_agg_pushdown;
SELECT current_setting('db9.enable_cop_agg_pushdown');
SELECT current_setting('db9.enable_cop_agg_pushdown', true);
SET db9.enable_cop_agg_pushdown = on;
RESET db9.enable_cop_agg_pushdown;
