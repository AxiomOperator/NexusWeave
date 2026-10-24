<template>
  <main>
    <UserCard :user="user" @save="reload" />
    <user-card :user="user" />
    <button @click="reload">reload</button>
  </main>
</template>

<script setup lang="ts">
import UserCard from './components/UserCard.vue';
import { fetchUser } from './api';

let user = fetchUser(1);

function reload() {
  user = fetchUser(1);
}
</script>
