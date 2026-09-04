<script lang="ts">
  import type { Snippet } from "svelte";
  import type { HTMLAttributes } from "svelte/elements";

  type Props = HTMLAttributes<HTMLDivElement> & { children: Snippet };
  let { children, class: className = "", ...rest }: Props = $props();
</script>

<div class={`rounded-lg border border-edge bg-panel ${className}`} {...rest}>
  {@render children()}
</div>
