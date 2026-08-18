export interface AiSearchQueryRewriteResult {
  query: string;
  keywords: string[];
  filters: Record<string, string | number | boolean | string[]>;
  assumptions: string[];
}

export interface AiCandidateSearchAdapter<TCandidate> {
  search(
    input: AiSearchQueryRewriteResult,
    limit: number,
  ): Promise<TCandidate[]>;
}

export interface AiRerankResult {
  id: string;
  score: number;
  reason: string;
}

export interface AiRerankAdapter<TCandidate> {
  rerank(query: string, candidates: TCandidate[]): Promise<AiRerankResult[]>;
}
