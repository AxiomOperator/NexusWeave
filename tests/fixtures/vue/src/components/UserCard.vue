<template>
  <div class="card">{{ user.id }}</div>
</template>

<script setup lang="ts">
import { saveUser } from '../api';

const props = defineProps<{ user: { id: number } }>();

function onSave() {
  saveUser(props.user);
}
</script>

<style scoped>
.card { color: red; }
</style>
