<script lang="ts">
  import type { Snippet } from "svelte";

  type Tone = "neutral" | "healthy" | "warning" | "danger";
  type Props = {
    children: Snippet;
    tone?: Tone;
  };

  let { children, tone = "neutral" }: Props = $props();

  const tones: Record<Tone, string> = {
    neutral: "border-edge-strong bg-white/4 text-ink-secondary",
    healthy: "border-online/25 bg-online/8 text-online",
    warning: "border-warning/25 bg-warning/8 text-warning",
    danger: "border-danger/25 bg-danger/8 text-danger",
  };
</script>

<span
  class={`inline-flex items-center rounded-full border px-2 py-1 font-mono text-[0.6875rem] leading-4 font-medium tracking-wide uppercase ${tones[tone]}`}
>
  {@render children()}
</span>
