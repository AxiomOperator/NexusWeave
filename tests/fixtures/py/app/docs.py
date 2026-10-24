from mongoengine import Document


class Profile(Document):
    def bio(self):
        return ""


def load_bio(pid):
    return Profile.objects.filter(owner=pid).get().bio()
