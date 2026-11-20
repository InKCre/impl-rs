INSERT INTO inkcre.block_lexical_records(block,label,text,search_vector) VALUES($1,$2,$3,setweight(to_tsvector('simple',$2),'A') || setweight(to_tsvector('simple',coalesce($3,'')),'D'))
ON CONFLICT (block) DO UPDATE SET label=EXCLUDED.label,text=EXCLUDED.text,search_vector=EXCLUDED.search_vector,updated_at=CURRENT_TIMESTAMP
