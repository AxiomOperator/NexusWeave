<script lang="ts">
  import Item from './Item.svelte';
  import { fetchItems } from './api';

  let items = fetchItems();

  function refresh() {
    items = fetchItems();
  }
</script>

<button on:click={refresh}>refresh</button>
<ul>
  {#each items as it}
    <Item name={it} />
  {/each}
</ul>

<style>
  ul { margin: 0; }
</style>
