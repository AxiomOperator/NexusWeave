from django.db import models


class Article(models.Model):
    def summary(self):
        return "s"


def latest_summary():
    return Article.objects.filter(published=True).order_by("-id").first().summary()
