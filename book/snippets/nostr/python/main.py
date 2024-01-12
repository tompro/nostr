#!/usr/bin/env python

from src.keys import keys
from src.event.json import event_json
from src.event.builder import event_builder

 
def main():  
   keys()
   event_json()
   event_builder()

main()