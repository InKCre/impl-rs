WITH candidates AS (SELECT b.*,r.label,r.text, CASE WHEN lower(r.label)=lower($1) THEN 'label_exact' WHEN r.label ILIKE $2 ESCAPE '\' THEN 'label_substring' WHEN r.text ILIKE $2 ESCAPE '\' THEN 'text_substring' ELSE 'terms' END AS evidence,CASE WHEN lower(r.label)=lower($1) THEN 4.0 WHEN r.label ILIKE $2 ESCAPE '\' THEN 3.0 WHEN r.text ILIKE $2 ESCAPE '\' THEN 2.0 ELSE 1.0 END AS class, ts_rank_cd(r.search_vector,plainto_tsquery('simple',$1)) AS terms
FROM inkcre.blocks b
JOIN inkcre.block_lexical_records r ON r.block=b.id
WHERE r.updated_at>=b.updated_at
AND ($3::text IS NULL OR b.resolver=$3)
AND ($4::integer IS NULL OR b.storage=$4)
AND (lower(r.label)=lower($1) OR r.label ILIKE $2 ESCAPE '\' OR r.text ILIKE $2 ESCAPE '\' OR r.search_vector @@ plainto_tsquery('simple',$1)))
SELECT *, (class+terms)::double precision AS rank
FROM candidates
ORDER BY class DESC,terms DESC,id
LIMIT $5
