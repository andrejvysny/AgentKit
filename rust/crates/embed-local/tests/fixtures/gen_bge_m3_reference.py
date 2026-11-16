import json, sys
from sentence_transformers import SentenceTransformer
MAX = 256
para = ("The quick brown fox jumps over the lazy dog while the committee reviews the quarterly infrastructure budget, "
        "debates retention policies for long-term memory, and schedules the next migration of the vector index. ")
inputs = [
 ("short_en", "Hello world."),
 ("question_en", "How do I rotate the signing key for the staging cluster?"),
 ("medium_en", "Embeddings map text to vectors so that semantically similar passages end up close together; retrieval systems rank stored memories by cosine similarity against the query vector."),
 ("slovak", "Dnes ráno sme v Bratislave stretli kamaráta a dlho sme sa rozprávali o novej práci, o počasí a o tom, kde stráviť leto."),
 ("slovak_diacritics", "Žltý kôň úpel ďábelské ódy, ľúbezne šušťal čučoriedky v Štrbskom Plese."),
 ("czech", "Příliš žluťoučký kůň úpěl ďábelské ódy a zítra pojedeme vlakem do Brna na poradu o rozpočtu."),
 ("emoji", "Great job team 🎉🚀 shipping the release on time 😄👍"),
 ("code", "fn main() { println!(\"{}\", 1 + 1); } // prints two"),
 ("single_char", "a"),
 ("numbers_punct", "Invoice #4821 - total: 1,299.50 EUR (due 2026-12-31)!"),
 ("long_truncated", para * 12),
 ("mixed_long_sk", ("Dlhý text v slovenčine o pamäti agenta a vyhľadávaní. " * 40)),
]
m = SentenceTransformer("BAAI/bge-m3", device="cpu")
m.max_seq_length = MAX
texts = [t for _, t in inputs]
tok = m.tokenizer
lens = [len(tok(t, truncation=False)["input_ids"]) for t in texts]
vecs = m.encode(texts, batch_size=16, normalize_embeddings=True, convert_to_numpy=True)
assert vecs.shape[1] == 1024
out = {
  "model": "BAAI/bge-m3", "max_tokens": MAX, "pooling": "cls", "normalize": True,
  "generator": "sentence-transformers (fp32, CPU), encode(normalize_embeddings=True), max_seq_length=256",
  "cases": [{"name": n, "text": t, "tokens_untruncated": l, "vector": [round(float(x), 6) for x in v]}
            for (n, t), l, v in zip(inputs, lens, vecs)],
}
json.dump(out, open(sys.argv[1], "w"), ensure_ascii=False, separators=(",", ":"))
print(lens)
