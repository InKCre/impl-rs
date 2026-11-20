SELECT b.*
FROM inkcre.blocks b
LEFT
JOIN inkcre.block_lexical_records r ON r.block=b.id
WHERE b.id>$1
AND (r.block IS NULL OR r.updated_at<b.updated_at OR ($2::timestamptz IS NOT NULL
AND r.updated_at<$2))
ORDER BY b.id
LIMIT $3
