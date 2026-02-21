-- Default GUC values without prior SET
SHOW extra_float_digits;
SHOW bytea_output;
SHOW lc_messages;
SHOW lc_monetary;
SHOW lc_numeric;
SHOW lc_time;
SHOW max_identifier_length;
SHOW max_index_keys;
SHOW work_mem;
SHOW in_hot_standby;
SHOW password_encryption;

-- SET overrides default
SET extra_float_digits = 3;
SHOW extra_float_digits;

-- RESET restores default
RESET extra_float_digits;
SHOW extra_float_digits;

-- current_setting() also works via same show_value() path
SELECT current_setting('bytea_output');
SELECT current_setting('max_identifier_length');

-- Already-handled GUCs still work
SHOW datestyle;
SHOW server_encoding;
SHOW integer_datetimes;

-- Unknown GUC still errors
SHOW nonexistent_setting_xyz;
